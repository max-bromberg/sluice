//! An in-memory backend, so the policy engine can be tested without a distro.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{LockSpec, PackageBackend, Pkg, PkgStatus, Transaction};
use crate::exec::Runner;

#[derive(Default)]
pub struct MockBackend {
    pub packages: RefCell<Vec<Pkg>>,
    pub locks: RefCell<Vec<LockSpec>>,
    pub refreshed: RefCell<u32>,
    /// Every `install_exact` call, for asserting the family moved as one unit.
    pub installs: RefCell<Vec<Vec<String>>>,
    /// Every `remove_exact` call.
    pub removals: RefCell<Vec<Vec<String>>>,
    pub dups: RefCell<u32>,
    pub changelogs: RefCell<std::collections::BTreeMap<String, String>>,
    /// Registered local repositories: alias -> (directory, priority).
    pub repos: RefCell<std::collections::BTreeMap<String, (PathBuf, u32)>>,
}

impl MockBackend {
    pub fn with_packages(packages: Vec<Pkg>) -> Self {
        MockBackend {
            packages: RefCell::new(packages),
            ..Default::default()
        }
    }

    pub fn lock_specs(&self) -> Vec<String> {
        self.locks
            .borrow()
            .iter()
            .map(LockSpec::spec_string)
            .collect()
    }
}

/// Glob matching for the subset zypper's search patterns use: `*` and `?`.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
            Some(b'?') => !t.is_empty() && inner(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && inner(&p[1..], &t[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

impl PackageBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn refresh(&self, _r: &mut Runner) -> Result<()> {
        *self.refreshed.borrow_mut() += 1;
        Ok(())
    }

    fn query(&self, _r: &mut Runner, pattern: &str) -> Result<Vec<Pkg>> {
        Ok(self
            .packages
            .borrow()
            .iter()
            .filter(|p| glob_match(pattern, &p.name))
            .cloned()
            .collect())
    }

    fn installed_details(
        &self,
        _r: &mut Runner,
        names: &[String],
    ) -> Result<Vec<super::InstalledPkg>> {
        Ok(self
            .packages
            .borrow()
            .iter()
            .filter(|p| p.installed())
            .filter(|p| names.is_empty() || names.contains(&p.name))
            .map(|p| super::InstalledPkg {
                name: p.name.clone(),
                evr: p.evr.clone(),
                source: p.source.clone(),
                installed_at: None,
            })
            .collect())
    }

    fn locks(&self, _r: &mut Runner) -> Result<Vec<LockSpec>> {
        Ok(self.locks.borrow().clone())
    }

    fn add_lock(&self, _r: &mut Runner, spec: &LockSpec) -> Result<()> {
        let mut locks = self.locks.borrow_mut();
        if !locks.contains(spec) {
            locks.push(spec.clone());
        }
        Ok(())
    }

    fn remove_lock(&self, _r: &mut Runner, spec: &LockSpec) -> Result<()> {
        self.locks.borrow_mut().retain(|l| l != spec);
        Ok(())
    }

    fn dist_upgrade(&self, _r: &mut Runner, _extra: &[String]) -> Result<Transaction> {
        *self.dups.borrow_mut() += 1;
        Ok(Transaction::default())
    }

    fn install_exact(&self, _r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        self.installs
            .borrow_mut()
            .push(pkgs.iter().map(Pkg::exact_spec).collect());

        // Reflect the install back into the package set, the way a real
        // transaction would: the new version becomes installed.
        let mut all = self.packages.borrow_mut();
        for pkg in pkgs {
            for existing in all.iter_mut() {
                if existing.name == pkg.name && existing.evr == pkg.evr {
                    existing.status = PkgStatus::Installed;
                }
            }
        }
        Ok(Transaction {
            installed: pkgs.to_vec(),
            removed: Vec::new(),
            output: String::new(),
        })
    }

    fn remove_exact(&self, _r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        self.removals
            .borrow_mut()
            .push(pkgs.iter().map(Pkg::exact_spec).collect());
        let mut all = self.packages.borrow_mut();
        for pkg in pkgs {
            for existing in all.iter_mut() {
                if existing.name == pkg.name && existing.evr == pkg.evr {
                    existing.status = PkgStatus::Available;
                }
            }
        }
        Ok(Transaction {
            installed: Vec::new(),
            removed: pkgs.to_vec(),
            output: String::new(),
        })
    }

    fn fetch_rpms(&self, _r: &mut Runner, pkgs: &[Pkg], dest: &Path) -> Result<Vec<PathBuf>> {
        // Like the real thing, only what a repository still carries can be
        // downloaded; an installed version the repo has dropped cannot.
        let all = self.packages.borrow();
        let missing: Vec<String> = pkgs
            .iter()
            .filter(|p| {
                !all.iter()
                    .any(|a| a.name == p.name && a.evr == p.evr && a.offered())
            })
            .map(Pkg::nevra)
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "the repositories did not supply: {}",
            missing.join(", ")
        );
        std::fs::create_dir_all(dest)?;
        let mut out = Vec::new();
        for p in pkgs {
            let path = dest.join(format!("{}.rpm", p.nevra()));
            std::fs::write(&path, b"mock rpm")?;
            out.push(path);
        }
        Ok(out)
    }

    fn changelog(&self, _r: &mut Runner, pkg: &Pkg) -> Result<Option<String>> {
        Ok(self.changelogs.borrow().get(&pkg.nevra()).cloned())
    }

    fn file_changelog(&self, _r: &mut Runner, path: &Path) -> Result<Option<String>> {
        let nevra = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".rpm"))
            .unwrap_or_default();
        Ok(self.changelogs.borrow().get(nevra).cloned())
    }

    fn ensure_local_repo(
        &self,
        _r: &mut Runner,
        alias: &str,
        dir: &Path,
        priority: u32,
    ) -> Result<bool> {
        let mut repos = self.repos.borrow_mut();
        if repos.contains_key(alias) {
            return Ok(false);
        }
        repos.insert(alias.to_string(), (dir.to_path_buf(), priority));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_zypper_style_patterns() {
        assert!(glob_match("kernel-default", "kernel-default"));
        assert!(glob_match("kernel*", "kernel-default-devel"));
        assert!(glob_match("Mesa*", "Mesa-dri"));
        assert!(!glob_match("Mesa*", "libMesa"));
        assert!(glob_match("*firmware*", "kernel-firmware-amdgpu"));
        assert!(glob_match("kernel-?", "kernel-x"));
        assert!(!glob_match("kernel-?", "kernel-xy"));
    }
}
