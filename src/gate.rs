//! Holding and releasing package locks safely.
//!
//! Two mechanisms, in order of preference:
//!
//! 1. A **version-conditional lock** (`kernel-default >= 7.3`). Fix releases
//!    inside the held series are never locked, so there is no unlock window at
//!    all and nothing to get wrong. libzypp supports this; see
//!    `docs/verification.md`.
//!
//! 2. A **controlled unlock window**, for backends or situations where a
//!    conditional lock is not usable. This is the dangerous path, so it is
//!    guarded three ways: a `Drop` impl covers returns and panics, a signal
//!    handler covers SIGINT and SIGTERM, and an on-disk journal covers the
//!    cases neither can — SIGKILL, a power loss, an OOM kill. The journal is
//!    replayed on the next run, so a machine can never be left silently
//!    unlocked.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backend::{CmpOp, LockSpec, PackageBackend};
use crate::exec::Runner;
use crate::version::Evr;

/// The lock that holds a component to its current series: everything at or
/// above the *next* series is refused, everything below flows.
pub fn series_lock(package: &str, next_series: &str, component: &str) -> LockSpec {
    LockSpec::bounded(package, CmpOp::Ge, Evr::parse(next_series))
        .with_comment(format!("sluice: {component} series gate"))
}

/// The series immediately after `series`, by incrementing its last numeric
/// component: `7.2` -> `7.3`, `26.2` -> `26.3`.
pub fn next_series(series: &str) -> Option<String> {
    let (head, tail) = match series.rsplit_once('.') {
        Some((h, t)) => (Some(h), t),
        None => (None, series),
    };
    let n: u64 = tail.parse().ok()?;
    Some(match head {
        Some(h) => format!("{h}.{}", n + 1),
        None => (n + 1).to_string(),
    })
}

// ---------------------------------------------------------------------------
// Crash-safe unlock window
// ---------------------------------------------------------------------------

/// Written to disk for as long as locks are removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRestore {
    pub opened_at: DateTime<Utc>,
    pub pid: u32,
    pub locks: Vec<LockSpec>,
    pub reason: String,
}

pub fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join("lock-restore.json")
}

/// Replay an interrupted unlock window, if one is recorded.
///
/// Called at startup before anything else touches packages. Re-adding a lock
/// that is already present is harmless, so this is safe to run unconditionally.
pub fn recover(
    backend: &dyn PackageBackend,
    r: &mut Runner,
    state_dir: &Path,
) -> Result<Option<PendingRestore>> {
    let path = journal_path(state_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let pending: PendingRestore = serde_json::from_str(&text)
        .with_context(|| format!("parsing interrupted lock journal {}", path.display()))?;

    let existing = backend.locks(r)?;
    for spec in &pending.locks {
        if !existing.contains(spec) {
            backend.add_lock(r, spec)?;
        }
    }
    r.note(&format!(
        "recovered {} lock(s) from an interrupted `{}` window opened at {}",
        pending.locks.len(),
        pending.reason,
        pending.opened_at
    ));
    let _ = std::fs::remove_file(&path);
    Ok(Some(pending))
}

/// An open unlock window. Restores the locks when dropped, whatever the reason.
pub struct LockWindow<'a> {
    backend: &'a dyn PackageBackend,
    locks: Vec<LockSpec>,
    journal: PathBuf,
    interrupted: Arc<AtomicBool>,
    restored: bool,
}

impl<'a> LockWindow<'a> {
    /// Remove `locks`, recording them first so they can be restored even if
    /// this process dies without running any more code.
    pub fn open(
        backend: &'a dyn PackageBackend,
        r: &mut Runner,
        state_dir: &Path,
        locks: Vec<LockSpec>,
        reason: &str,
    ) -> Result<Self> {
        let journal = journal_path(state_dir);
        let pending = PendingRestore {
            opened_at: Utc::now(),
            pid: std::process::id(),
            locks: locks.clone(),
            reason: reason.to_string(),
        };

        // The journal is written *before* the first lock comes off, and fsynced,
        // so there is no instant at which a lock is removed without a record of
        // how to put it back.
        write_journal(&journal, &pending)
            .with_context(|| format!("recording lock journal at {}", journal.display()))?;

        let interrupted = Arc::new(AtomicBool::new(false));
        for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            // Registration failure is not fatal: Drop and the journal still
            // cover us. It does mean Ctrl-C will not restore promptly, so say so.
            if let Err(e) = signal_hook::flag::register(sig, Arc::clone(&interrupted)) {
                eprintln!("warning: could not install signal handler ({e}); locks will still be restored on exit");
            }
        }

        for spec in &locks {
            backend.remove_lock(r, spec)?;
        }

        Ok(LockWindow {
            backend,
            locks,
            journal,
            interrupted,
            restored: false,
        })
    }

    /// True once SIGINT or SIGTERM has arrived. Long operations poll this and
    /// stop at a safe point rather than being killed mid-transaction.
    pub fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }

    pub fn check_interrupted(&self) -> Result<()> {
        anyhow::ensure!(!self.interrupted(), "interrupted; restoring locks");
        Ok(())
    }

    /// Restore the locks and verify they are actually back.
    pub fn close(mut self, r: &mut Runner) -> Result<()> {
        self.restore(r)?;

        let present = self.backend.locks(r)?;
        let missing: Vec<String> = self
            .locks
            .iter()
            .filter(|l| !present.contains(l))
            .map(LockSpec::spec_string)
            .collect();
        anyhow::ensure!(
            missing.is_empty() || r.dry_run(),
            "locks were not restored: {}. The system is UNLOCKED; re-run `sluice repair`.",
            missing.join(", ")
        );
        Ok(())
    }

    fn restore(&mut self, r: &mut Runner) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let mut first_error = None;
        for spec in &self.locks {
            // Every lock is attempted even if an earlier one failed: a partial
            // restore is better than stopping at the first problem.
            if let Err(e) = self.backend.add_lock(r, spec) {
                first_error.get_or_insert(e);
            }
        }
        let _ = std::fs::remove_file(&self.journal);
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for LockWindow<'_> {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        // Reached on an early return, a `?`, or a panic. There is no Runner
        // here, so a fresh one is used; it deliberately has no log file, since
        // the journal on disk is the durable record.
        let mut r = Runner::new(false, None);
        if let Err(e) = self.restore(&mut r) {
            eprintln!(
                "ERROR: could not restore package locks: {e:#}\n\
                 The system is UNLOCKED. Run `sluice repair` before your next upgrade."
            );
        }
    }
}

fn write_journal(path: &Path, pending: &PendingRestore) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(serde_json::to_string_pretty(pending)?.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn next_series_increments_the_last_component() {
        assert_eq!(next_series("7.2").as_deref(), Some("7.3"));
        assert_eq!(next_series("7.9").as_deref(), Some("7.10"));
        assert_eq!(next_series("26.2").as_deref(), Some("26.3"));
        assert_eq!(next_series("7").as_deref(), Some("8"));
        assert_eq!(next_series("20260829"), Some("20260830".into()));
        assert_eq!(next_series("abc"), None);
    }

    /// The elegant path: hold 7.2 by locking everything from 7.3 up, so 7.2.6
    /// installs with no unlock window at all.
    #[test]
    fn series_lock_bounds_at_the_next_series() {
        let lock = series_lock("kernel-default", "7.3", "kernel");
        assert_eq!(lock.spec_string(), "kernel-default >= 7.3");
        assert!(!lock.is_blanket());
        assert_eq!(lock.comment.as_deref(), Some("sluice: kernel series gate"));
    }

    fn lock() -> LockSpec {
        series_lock("kernel-default", "7.3", "kernel")
    }

    #[test]
    fn closing_a_window_restores_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);
        backend.add_lock(&mut r, &lock()).unwrap();

        let w = LockWindow::open(&backend, &mut r, dir.path(), vec![lock()], "test").unwrap();
        assert!(
            backend.lock_specs().is_empty(),
            "the window must actually unlock"
        );
        assert!(
            journal_path(dir.path()).exists(),
            "the journal must exist while unlocked"
        );

        w.close(&mut r).unwrap();
        assert_eq!(backend.lock_specs(), vec!["kernel-default >= 7.3"]);
        assert!(
            !journal_path(dir.path()).exists(),
            "the journal must be cleared on close"
        );
    }

    /// The acceptance test from the spec: an early exit mid-window must still
    /// leave the locks in place.
    #[test]
    fn dropping_a_window_without_closing_restores_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);
        backend.add_lock(&mut r, &lock()).unwrap();

        {
            let _w = LockWindow::open(&backend, &mut r, dir.path(), vec![lock()], "test").unwrap();
            assert!(backend.lock_specs().is_empty());
            // Fall out of scope as an error path would.
        }

        assert_eq!(backend.lock_specs(), vec!["kernel-default >= 7.3"]);
        assert!(!journal_path(dir.path()).exists());
    }

    /// SIGKILL leaves no chance to run Drop. The journal is what covers it.
    #[test]
    fn an_abandoned_journal_is_replayed_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);

        write_journal(
            &journal_path(dir.path()),
            &PendingRestore {
                opened_at: Utc::now(),
                pid: 1234,
                locks: vec![lock()],
                reason: "update".into(),
            },
        )
        .unwrap();

        let recovered = recover(&backend, &mut r, dir.path()).unwrap().unwrap();
        assert_eq!(recovered.locks.len(), 1);
        assert_eq!(backend.lock_specs(), vec!["kernel-default >= 7.3"]);
        assert!(!journal_path(dir.path()).exists());
    }

    #[test]
    fn recovery_is_a_no_op_when_nothing_was_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);
        assert!(recover(&backend, &mut r, dir.path()).unwrap().is_none());
        assert!(backend.lock_specs().is_empty());
    }

    #[test]
    fn recovery_does_not_duplicate_locks_that_are_already_present() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::default();
        let mut r = Runner::new(false, None);
        backend.add_lock(&mut r, &lock()).unwrap();
        write_journal(
            &journal_path(dir.path()),
            &PendingRestore {
                opened_at: Utc::now(),
                pid: 1,
                locks: vec![lock()],
                reason: "update".into(),
            },
        )
        .unwrap();

        recover(&backend, &mut r, dir.path()).unwrap();
        assert_eq!(backend.lock_specs().len(), 1);
    }
}
