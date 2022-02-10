//! AUR package dependency solving.

use alpm::Alpm;
use alpm_utils::DbListExt;
use chrono::Utc;
use log::{debug, info};
use nonempty::NonEmpty;
use r2d2::{ManageConnection, Pool};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use srcinfo::Srcinfo;
use std::borrow::{Borrow, Cow};
use std::collections::HashSet;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use validated::Validated;

/// Errors that can occur during dependency resolution.
pub enum Error {
    /// A [`Mutex`] was poisoned and couldn't be unlocked.
    PoisonedMutex,
    /// An error pulling from the resource pool.
    R2D2(r2d2::Error),
    /// An error parsing a `.SRCINFO` file.
    Srcinfo(srcinfo::Error),
    /// An error cloning or pulling a repo.
    Git(crate::git::Error),
    /// An error contacting the AUR API.
    Raur(raur_curl::Error),
    /// Multiple errors during concurrent dependency resolution.
    Resolutions(Box<NonEmpty<Error>>),
    /// A named dependency does not exist.
    DoesntExist(String),
    /// A named dependency of some known package does not exist.
    DoesntExistWithParent(String, String),
}

impl From<raur_curl::Error> for Error {
    fn from(v: raur_curl::Error) -> Self {
        Self::Raur(v)
    }
}

impl From<crate::git::Error> for Error {
    fn from(v: crate::git::Error) -> Self {
        Self::Git(v)
    }
}

impl From<srcinfo::Error> for Error {
    fn from(v: srcinfo::Error) -> Self {
        Self::Srcinfo(v)
    }
}

impl From<r2d2::Error> for Error {
    fn from(v: r2d2::Error) -> Self {
        Self::R2D2(v)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::PoisonedMutex => write!(f, "Poisoned Mutex"),
            Error::R2D2(e) => write!(f, "{}", e),
            Error::Resolutions(es) => {
                writeln!(f, "Errors during dependency resolution:")?;
                for e in (*es).iter() {
                    writeln!(f, "{}", e)?;
                }
                Ok(())
            }
            Error::Srcinfo(e) => write!(f, "{}", e),
            Error::Git(e) => write!(f, "{}", e),
            Error::Raur(e) => write!(f, "{}", e),
            Error::DoesntExist(p) => write!(f, "{} is not a known package.", p),
            Error::DoesntExistWithParent(par, p) => {
                write!(f, "{}, required by {}, is not a known package.", p, par)
            }
        }
    }
}

/// The results of dependency resolution.
#[derive(Default)]
pub struct Resolution<'a> {
    /// Packages to be installed from official repos.
    pub to_install: HashSet<Official<'a>>,
    /// Packages to be built.
    pub to_build: HashSet<Buildable<'a>>,
    /// Packages already installed on the system.
    pub satisfied: HashSet<Cow<'a, str>>,
    /// Packages that are somehow accounted for. A dependency might be provided
    /// by some package, but under a slightly different name. This also takes
    /// split packages into account.
    provided: HashSet<Cow<'a, str>>,
}

impl Resolution<'_> {
    /// Have we already considered the given package?
    pub fn seen(&self, pkg: &str) -> bool {
        self.provided.contains(pkg)
            || self.satisfied.contains(pkg)
            || self.to_install.contains(pkg)
            || self.to_build.contains(pkg)
    }
}

/// An official ALPM package.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Official<'a>(Cow<'a, str>);

impl Borrow<str> for Official<'_> {
    fn borrow(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for Official<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A buildable package from the AUR.
#[derive(PartialEq, Eq)]
pub struct Buildable<'a> {
    /// The name of the AUR package.
    pub name: Cow<'a, str>,
    /// The names of its dependencies.
    pub deps: HashSet<Cow<'a, str>>,
}

impl Borrow<str> for Buildable<'_> {
    fn borrow(&self) -> &str {
        self.name.as_ref()
    }
}

impl Hash for Buildable<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

/// Determine all packages to be built and installed.
pub fn resolve<'a, I, S, M>(
    pool: Pool<M>,
    clone_dir: &Path,
    pkgs: I,
) -> Result<Resolution<'a>, Error>
where
    I: IntoParallelIterator<Item = S>,
    S: Into<Cow<'a, str>>,
    M: ManageConnection<Connection = Alpm>,
{
    let arc = Arc::new(Mutex::new(Resolution::default()));

    let start = Utc::now();
    pkgs.into_par_iter()
        .map(|pkg| resolve_one(pool.clone(), arc.clone(), clone_dir, None, pkg))
        .collect::<Validated<(), Error>>()
        .ok()
        .map_err(|es| Error::Resolutions(Box::new(es)))?;
    let end = Utc::now();
    let diff = end.timestamp_millis() - start.timestamp_millis();

    let res = Arc::try_unwrap(arc)
        .map_err(|_| Error::PoisonedMutex)?
        .into_inner()
        .map_err(|_| Error::PoisonedMutex)?;

    info!("Resolved dependencies in {}ms.", diff);

    Ok(res)
}

fn resolve_one<'a, S, M>(
    pool: Pool<M>,
    mutx: Arc<Mutex<Resolution<'a>>>,
    clone_dir: &Path,
    parent: Option<&str>,
    pkg: S,
) -> Result<(), Error>
where
    S: Into<Cow<'a, str>>,
    M: ManageConnection<Connection = Alpm>,
{
    let p = pkg.into();
    let pr = p.as_ref();

    // Drops the lock on the `Resolution` as soon as it can.
    let already_seen = {
        let res = mutx.lock().map_err(|_| Error::PoisonedMutex)?;
        res.seen(&p)
    };

    if !already_seen {
        debug!("Resolving dependencies for: {}", p);

        // Checks if the current package is installed or otherwise satisfied by
        // some package, and then immediately drops the ALPM handle.
        let satisfied = {
            let alpm = pool.get()?;
            let db = alpm.localdb();
            let start = Utc::now();
            let res = db.pkg(pr).is_ok() || db.pkgs().find_satisfier(pr).is_some();
            let end = Utc::now();
            let diff = end.timestamp_millis() - start.timestamp_millis();
            debug!("AlpmList::find_satisfier for {} in {}ms", pr, diff);
            res
        };

        if satisfied {
            mutx.lock()
                .map_err(|_| Error::PoisonedMutex)?
                .satisfied
                .insert(p);
        } else {
            // Same here, re: lock dropping.
            // TODO Wed Feb  9 22:41:15 2022
            //
            // Also need to do `find_satisfier` here!
            let official = pool.get()?.syncdbs().pkg(pr).is_ok();

            if official {
                // TODO Wed Feb  9 22:10:23 2022
                //
                // Recurse on the package for its dependencies.
                mutx.lock()
                    .map_err(|_| Error::PoisonedMutex)?
                    .to_install
                    .insert(Official(p));
            } else {
                debug!("It's an AUR package!");
                let path = pull_or_clone(clone_dir, parent, &p)?;
                debug!("Parsing .SRCINFO for {}", p);
                let info = Srcinfo::parse_file(path.join(".SRCINFO"))?;
                let name: Cow<'_, str> = Cow::Owned(info.base.pkgbase);
                let mut prov = Vec::new();
                let deps: HashSet<Cow<'_, str>> = info
                    .base
                    .makedepends
                    .into_iter()
                    .chain(info.pkg.depends)
                    .chain(
                        info.pkgs
                            .into_iter()
                            .map(|p| {
                                prov.push(p.pkgname);
                                p.depends
                            })
                            .flatten(),
                    )
                    .flat_map(|av| av.vec)
                    .map(Cow::Owned)
                    .collect();

                // TODO Try Cow tricks instead?
                let deps_copy: Vec<&str> = deps.iter().map(|d| d.as_ref()).collect();
                let parent = name.as_ref();
                let buildable = Buildable { name, deps };

                // mutx.lock().map_err(|_| Error::PoisonedMutex).map(|mut r| {
                //     r.to_build.insert(buildable);

                //     for p in info
                //         .pkg
                //         .provides
                //         .into_iter()
                //         .flat_map(|av| av.vec)
                //         .chain(prov)
                //     {
                //         r.provided.insert(Cow::Owned(p));
                //     }
                // })?;

                // deps_copy
                //     .into_iter()
                //     .map(|pkg| {
                //         resolve_one(pool.clone(), mutx.clone(), clone_dir, Some(parent), pkg)
                //     })
                //     .collect::<Validated<(), Error>>()
                //     .ok()
                //     .map_err(|es| Error::Resolutions(Box::new(es)))?;
            }
        }
    }

    Ok(())
}

// FIXME Mon Feb  7 23:07:56 2022
//
// If `is_aur_package_fast` succeeds, perhaps we should assume that the clone is
// up to date and avoid a pull here to speed things up. It may be better to
// encourage usage of `-Ay`.
//
// Of course if there is no local clone, then a fresh one must be done either
// way, ensuring newness for at least that run.
//
// The goal here is to rely on our local clone more, to avoid having to call to
// the AUR all the time. `-Ai`, perhaps, should also read local clones if they
// exist. This offers the bonus of `-Ai` functioning offline, like `-Si` does!
fn pull_or_clone(clone_dir: &Path, parent: Option<&str>, pkg: &str) -> Result<PathBuf, Error> {
    if super::is_aur_package_fast(clone_dir, pkg) {
        let path = clone_dir.join(pkg);
        // crate::git::pull(&path)?; // Here. Potentially avoid this.
        Ok(path)
    } else {
        let info = crate::aur::info(&[pkg])?;
        let base = info
            .first()
            .ok_or_else(|| match parent {
                Some(par) => Error::DoesntExistWithParent(par.to_string(), pkg.to_string()),
                None => Error::DoesntExist(pkg.to_string()),
            })?
            .package_base
            .as_str();

        // FIXME Wed Feb  9 21:24:27 2022
        //
        // Avoid the code duplication.
        if super::is_aur_package_fast(clone_dir, base) {
            let path = clone_dir.join(base);
            // crate::git::pull(&path)?; // Here. Potentially avoid this.
            Ok(path)
        } else {
            let path = crate::aur::clone_aur_repo(Some(clone_dir), base)?;
            Ok(path)
        }
    }
}

/// Given a collection of [`Buildable`] packages, determine a tiered order in
/// which they should be built and installed together.
///
/// This ensures that all dependencies are built and installed before they're
/// needed.
pub fn build_order<'a, I>(to_build: I) -> Vec<Vec<Cow<'a, str>>>
where
    I: IntoIterator<Item = Buildable<'a>>,
{
    info!("Determining build order.");

    todo!()
}
