//! The local RPM vault.
//!
//! On a rolling distribution the repository only carries the present. Once
//! Tumbleweed moves to 7.3, the 7.2.6 RPMs are gone, and with them any ability
//! to go back. The vault is a plain directory of kept RPMs, registered as a
//! low-priority zypper repository, so a rollback target still exists after the
//! repository has forgotten it.
//!
//! Priority matters: the vault is registered *below* the distribution
//! repositories so it never wins ordinary resolution. It exists to be asked
//! for explicitly, not to quietly hold the system back.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::backend::{PackageBackend, Pkg};
use crate::exec::Runner;
use crate::state::{ComponentState, VaultEntry};
use crate::version::Evr;

/// zypper priorities count *up* for lower precedence; the distribution repos
/// sit at 99, so anything above that loses to them.
pub const VAULT_PRIORITY: u32 = 200;
pub const VAULT_ALIAS: &str = "sluice-vault";

pub fn component_dir(vault_dir: &Path, component: &str, version: &Evr) -> PathBuf {
    vault_dir.join(component).join(version.to_string())
}

/// Download and keep the RPMs for `pkgs`, recording them in the component state.
pub fn store(
    backend: &dyn PackageBackend,
    r: &mut Runner,
    vault_dir: &Path,
    component: &str,
    version: &Evr,
    pkgs: &[Pkg],
    state: &mut ComponentState,
) -> Result<VaultEntry> {
    let dir = component_dir(vault_dir, component, version);
    let files = backend
        .fetch_rpms(r, pkgs, &dir)
        .with_context(|| format!("vaulting {component} {version}"))?;

    anyhow::ensure!(
        !files.is_empty() || r.dry_run(),
        "no RPMs were retrieved for {component} {version}; the rollback target is NOT protected"
    );

    let entry = VaultEntry {
        version: version.clone(),
        files,
        stored_at: Utc::now(),
    };
    state.vault.retain(|v| v.version != *version);
    state.vault.push(entry.clone());
    state.vault.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(entry)
}

/// Register the vault as a plain-directory repository, if it is not already.
pub fn ensure_registered(
    backend: &dyn PackageBackend,
    r: &mut Runner,
    vault_dir: &Path,
) -> Result<bool> {
    std::fs::create_dir_all(vault_dir)
        .with_context(|| format!("creating vault at {}", vault_dir.display()))?;
    backend.ensure_local_repo(r, VAULT_ALIAS, vault_dir, VAULT_PRIORITY)
}

/// Remove vaulted RPMs for versions that are neither known-good nor installed.
///
/// The known-good version is never a candidate for removal, whatever else is
/// passed in — that is the whole point of the vault.
pub fn prune(
    vault_dir: &Path,
    component: &str,
    state: &mut ComponentState,
    keep: &[Evr],
    dry_run: bool,
) -> Result<Vec<Evr>> {
    let mut protected: Vec<Evr> = keep.to_vec();
    if let Some(kg) = &state.known_good {
        protected.push(kg.clone());
    }
    if let Some(t) = &state.testing {
        protected.push(t.clone());
    }

    let doomed: Vec<Evr> = state
        .vault
        .iter()
        .map(|v| v.version.clone())
        .filter(|v| !protected.contains(v))
        .collect();

    if dry_run {
        return Ok(doomed);
    }

    for version in &doomed {
        let dir = component_dir(vault_dir, component, version);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
    }
    state.vault.retain(|v| protected.contains(&v.version));
    Ok(doomed)
}

/// Total bytes held in the vault, for the status display.
pub fn size_bytes(vault_dir: &Path) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| {
                let path = e.path();
                if path.is_dir() {
                    walk(&path)
                } else {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                }
            })
            .sum()
    }
    walk(vault_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{mock::MockBackend, PkgStatus};

    fn pkg(name: &str, evr: &str) -> Pkg {
        Pkg {
            name: name.into(),
            evr: Evr::parse(evr),
            arch: "x86_64".into(),
            repo: "repo-oss".into(),
            status: PkgStatus::Available,
            source: None,
        }
    }

    #[test]
    fn storing_records_files_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Runner::new(false, None);
        let mut state = ComponentState::default();
        let version = Evr::parse("7.2.6-1.1");
        let pkgs = vec![
            pkg("kernel-default", "7.2.6-1.1"),
            pkg("kernel-devel", "7.2.6-1.1"),
        ];
        let backend = MockBackend::with_packages(pkgs.clone());

        let entry = store(
            &backend,
            &mut r,
            dir.path(),
            "kernel",
            &version,
            &pkgs,
            &mut state,
        )
        .unwrap();
        assert_eq!(entry.files.len(), 2);
        assert!(state.is_vaulted(&version));

        // Vaulting the same version again replaces rather than duplicates.
        store(
            &backend,
            &mut r,
            dir.path(),
            "kernel",
            &version,
            &pkgs,
            &mut state,
        )
        .unwrap();
        assert_eq!(state.vault.len(), 1);
        assert!(size_bytes(dir.path()) > 0);
    }

    #[test]
    fn prune_never_removes_the_known_good_or_testing_version() {
        let dir = tempfile::tempdir().unwrap();
        let versions = ["7.0.12-1.1", "7.2.0-1.1", "7.2.6-1.1", "7.3.1-1.1"];
        let backend =
            MockBackend::with_packages(versions.iter().map(|v| pkg("kernel-default", v)).collect());
        let mut r = Runner::new(false, None);
        let mut state = ComponentState::default();

        for v in versions {
            store(
                &backend,
                &mut r,
                dir.path(),
                "kernel",
                &Evr::parse(v),
                &[pkg("kernel-default", v)],
                &mut state,
            )
            .unwrap();
        }
        state.known_good = Some(Evr::parse("7.0.12-1.1"));
        state.testing = Some(Evr::parse("7.3.1-1.1"));

        let removed = prune(
            dir.path(),
            "kernel",
            &mut state,
            &[Evr::parse("7.2.6-1.1")],
            false,
        )
        .unwrap();
        assert_eq!(removed, vec![Evr::parse("7.2.0-1.1")]);

        let kept: Vec<String> = state.vault.iter().map(|v| v.version.to_string()).collect();
        assert_eq!(kept, vec!["7.0.12-1.1", "7.2.6-1.1", "7.3.1-1.1"]);
        assert!(!component_dir(dir.path(), "kernel", &Evr::parse("7.2.0-1.1")).exists());
        assert!(component_dir(dir.path(), "kernel", &Evr::parse("7.0.12-1.1")).exists());
    }

    #[test]
    fn dry_run_prune_reports_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::with_packages(vec![pkg("kernel-default", "7.2.0-1.1")]);
        let mut r = Runner::new(false, None);
        let mut state = ComponentState::default();
        let v = Evr::parse("7.2.0-1.1");
        store(
            &backend,
            &mut r,
            dir.path(),
            "kernel",
            &v,
            &[pkg("kernel-default", "7.2.0-1.1")],
            &mut state,
        )
        .unwrap();

        let removed = prune(dir.path(), "kernel", &mut state, &[], true).unwrap();
        assert_eq!(removed, vec![v.clone()]);
        assert!(state.is_vaulted(&v), "dry run must not change state");
        assert!(component_dir(dir.path(), "kernel", &v).exists());
    }

    /// The vault exists for versions the repository may later drop, so it
    /// must not claim success for one it could not actually download.
    #[test]
    fn an_unobtainable_version_is_an_error_not_an_empty_vault() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);
        let mut state = ComponentState::default();
        let v = Evr::parse("7.2.0-1.1");
        assert!(store(
            &backend,
            &mut r,
            dir.path(),
            "kernel",
            &v,
            &[pkg("kernel-default", "7.2.0-1.1")],
            &mut state,
        )
        .is_err());
        assert!(!state.is_vaulted(&v));
    }

    #[test]
    fn vault_priority_loses_to_the_distribution_repositories() {
        // zypper treats a higher number as lower precedence; the distro repos
        // sit at 99, so the vault must be above that or it would hold updates back.
        const { assert!(VAULT_PRIORITY > 99) };
    }
}
