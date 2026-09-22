//! The package-manager seam.
//!
//! Everything sluice knows about installing, locking and querying packages goes
//! through [`PackageBackend`]. There is exactly one real implementation
//! ([`zypper`]), because zypper is the only package manager whose gating
//! semantics this tool has been tested against — but the seam keeps the policy
//! engine free of zypper strings and lets the tests run without a distro.

pub mod mock;
pub mod zypper;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::exec::Runner;
use crate::version::Evr;

/// Whether a package version is on the system or merely available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PkgStatus {
    Installed,
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pkg {
    pub name: String,
    pub evr: Evr,
    pub arch: String,
    pub repo: String,
    pub status: PkgStatus,
    /// The source package it was built from (`Mesa-drivers`), where known.
    /// Only installed packages carry it; see [`PackageBackend::installed_details`].
    #[serde(default)]
    pub source: Option<String>,
}

impl Pkg {
    pub fn installed(&self) -> bool {
        self.status == PkgStatus::Installed
    }

    /// Whether a configured repository carries this package. zypper reports
    /// installed packages no repository offers any more as `(System Packages)`.
    pub fn offered(&self) -> bool {
        self.repo != "(System Packages)"
    }

    /// `name-version-release.arch`, the form rpm prints and zypper accepts.
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr, self.arch)
    }

    /// `name=version-release`, zypper's exact-version install form.
    pub fn exact_spec(&self) -> String {
        format!("{}={}", self.name, self.evr)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Ge => ">=",
            CmpOp::Gt => ">",
        })
    }
}

/// A lock, optionally restricted to a version range.
///
/// Version-conditional locks are what make series holding clean: locking
/// `kernel-default >= 7.3` leaves every 7.2.x update free to flow, so there is
/// no unlock window to get wrong. See `docs/verification.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockSpec {
    pub name: String,
    pub bound: Option<(CmpOp, Evr)>,
    pub comment: Option<String>,
}

impl LockSpec {
    pub fn blanket(name: impl Into<String>) -> Self {
        LockSpec {
            name: name.into(),
            bound: None,
            comment: None,
        }
    }

    pub fn bounded(name: impl Into<String>, op: CmpOp, evr: Evr) -> Self {
        LockSpec {
            name: name.into(),
            bound: Some((op, evr)),
            comment: None,
        }
    }

    pub fn with_comment(mut self, c: impl Into<String>) -> Self {
        self.comment = Some(c.into());
        self
    }

    /// The `lock-spec` string zypper's `addlock`/`removelock` accept.
    pub fn spec_string(&self) -> String {
        match &self.bound {
            Some((op, evr)) => format!("{} {} {}", self.name, op, evr),
            None => self.name.clone(),
        }
    }

    pub fn is_blanket(&self) -> bool {
        self.bound.is_none()
    }
}

/// An installed package, as the package database records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPkg {
    pub name: String,
    pub evr: Evr,
    /// Source package name (`Mesa-drivers`), when known.
    pub source: Option<String>,
    pub installed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The outcome of a transaction, as reported back to the caller.
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    pub installed: Vec<Pkg>,
    pub removed: Vec<Pkg>,
    pub output: String,
    /// One line on what changed, e.g. `142 packages to upgrade, 3 new.`
    pub summary: Option<String>,
}

pub trait PackageBackend {
    fn name(&self) -> &'static str;

    /// Refresh repository metadata.
    fn refresh(&self, r: &mut Runner) -> Result<()>;

    /// Every known version of packages matching `pattern` (a glob), installed
    /// and available alike.
    fn query(&self, r: &mut Runner, pattern: &str) -> Result<Vec<Pkg>>;

    /// Every known version of packages matching any of `patterns`. Backends
    /// that can answer several patterns at once should override this.
    fn query_many(&self, r: &mut Runner, patterns: &[String]) -> Result<Vec<Pkg>> {
        let mut out: Vec<Pkg> = Vec::new();
        for p in patterns {
            for pkg in self.query(r, p)? {
                if !out.contains(&pkg) {
                    out.push(pkg);
                }
            }
        }
        Ok(out)
    }

    /// Every installed package with its source package and install time.
    /// The source is what groups subpackages built together, such as
    /// `libvulkan_radeon` with `Mesa-dri`, whatever their names.
    ///
    /// `names` limits the query to those packages; empty means all of them.
    fn installed_details(&self, r: &mut Runner, names: &[String]) -> Result<Vec<InstalledPkg>>;

    /// Installed packages matching `pattern`.
    fn installed(&self, r: &mut Runner, pattern: &str) -> Result<Vec<Pkg>> {
        Ok(self
            .query(r, pattern)?
            .into_iter()
            .filter(Pkg::installed)
            .collect())
    }

    fn locks(&self, r: &mut Runner) -> Result<Vec<LockSpec>>;
    fn add_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()>;
    fn remove_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()>;

    /// A full distribution upgrade, with whatever locks are currently in place.
    fn dist_upgrade(&self, r: &mut Runner, extra_args: &[String]) -> Result<Transaction>;

    /// Whether the package manager says a reboot is due: a core library or
    /// service was updated since boot.
    fn needs_reboot(&self, r: &mut Runner) -> Result<bool>;

    /// Install exactly these versions, as one transaction.
    fn install_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction>;

    /// Uninstall exactly these versions, as one transaction.
    fn remove_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction>;

    /// Download the RPMs for `pkgs` into `dest` without installing them.
    fn fetch_rpms(&self, r: &mut Runner, pkgs: &[Pkg], dest: &Path) -> Result<Vec<PathBuf>>;

    /// Changelog text for a specific package version, if the backend can get it.
    fn changelog(&self, r: &mut Runner, pkg: &Pkg) -> Result<Option<String>>;

    /// Changelog text of a downloaded package file.
    fn file_changelog(&self, r: &mut Runner, path: &Path) -> Result<Option<String>>;

    /// Register `dir` as a local, unsigned repository under `alias` with the
    /// given priority, unless one with that alias already exists. Returns
    /// whether it was newly added.
    fn ensure_local_repo(
        &self,
        r: &mut Runner,
        alias: &str,
        dir: &Path,
        priority: u32,
    ) -> Result<bool>;
}

/// Delegation through a shared handle, so a caller can keep inspecting a
/// backend after handing ownership of it to an [`crate::app::App`]. Used by the
/// tests; harmless in production.
impl<T: PackageBackend + ?Sized> PackageBackend for std::rc::Rc<T> {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn refresh(&self, r: &mut Runner) -> Result<()> {
        (**self).refresh(r)
    }
    fn query(&self, r: &mut Runner, pattern: &str) -> Result<Vec<Pkg>> {
        (**self).query(r, pattern)
    }
    fn query_many(&self, r: &mut Runner, patterns: &[String]) -> Result<Vec<Pkg>> {
        (**self).query_many(r, patterns)
    }
    fn installed_details(&self, r: &mut Runner, names: &[String]) -> Result<Vec<InstalledPkg>> {
        (**self).installed_details(r, names)
    }
    fn locks(&self, r: &mut Runner) -> Result<Vec<LockSpec>> {
        (**self).locks(r)
    }
    fn add_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()> {
        (**self).add_lock(r, spec)
    }
    fn remove_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()> {
        (**self).remove_lock(r, spec)
    }
    fn dist_upgrade(&self, r: &mut Runner, extra_args: &[String]) -> Result<Transaction> {
        (**self).dist_upgrade(r, extra_args)
    }
    fn needs_reboot(&self, r: &mut Runner) -> Result<bool> {
        (**self).needs_reboot(r)
    }
    fn install_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        (**self).install_exact(r, pkgs)
    }
    fn remove_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        (**self).remove_exact(r, pkgs)
    }
    fn fetch_rpms(&self, r: &mut Runner, pkgs: &[Pkg], dest: &Path) -> Result<Vec<PathBuf>> {
        (**self).fetch_rpms(r, pkgs, dest)
    }
    fn changelog(&self, r: &mut Runner, pkg: &Pkg) -> Result<Option<String>> {
        (**self).changelog(r, pkg)
    }
    fn file_changelog(&self, r: &mut Runner, path: &Path) -> Result<Option<String>> {
        (**self).file_changelog(r, path)
    }
    fn ensure_local_repo(
        &self,
        r: &mut Runner,
        alias: &str,
        dir: &Path,
        priority: u32,
    ) -> Result<bool> {
        (**self).ensure_local_repo(r, alias, dir, priority)
    }
}
