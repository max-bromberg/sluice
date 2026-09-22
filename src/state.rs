//! Persistent state: known-good versions, lifecycle, candidate first-seen dates.
//!
//! State is written atomically (temp file plus rename) because an interrupted
//! write that loses the known-good pin would strand the rollback target.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backend::LockSpec;
use crate::version::Evr;

/// Bumped when the on-disk shape changes incompatibly.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    /// Nothing has been vouched for yet: the state before the first `mark-good`.
    #[default]
    Unvetted,
    /// The installed version is one the user has vouched for.
    KnownGood,
    /// A promoted version is accumulating boot-health evidence.
    Testing,
}

impl Lifecycle {
    pub fn label(self) -> &'static str {
        match self {
            Lifecycle::Unvetted => "unvetted",
            Lifecycle::KnownGood => "known-good",
            Lifecycle::Testing => "testing",
        }
    }
}

/// A series that is available but held back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatedCandidate {
    pub series: String,
    pub version: Evr,
    pub first_seen: DateTime<Utc>,
    /// Set once the user has been told about this candidate, so `check` does
    /// not re-notify on every timer tick.
    pub notified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub version: Evr,
    pub files: Vec<PathBuf>,
    pub stored_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ComponentState {
    pub lifecycle: Lifecycle,
    /// The version the user has marked good. Deliberately *not* updated by
    /// within-series upgrades: a regression inside the series must still leave
    /// a target to fall back to.
    pub known_good: Option<Evr>,
    pub known_good_marked_at: Option<DateTime<Utc>>,
    /// The version currently under test, if `lifecycle` is `Testing`.
    pub testing: Option<Evr>,
    /// The series sluice is holding this component to. Set by `migrate`,
    /// `promote` and `rollback`; when unset, the series of the newest installed
    /// version is used. It is explicit because the newest installed version is
    /// not always the one you are on: after a rollback the tested version stays
    /// installed but outside the series.
    pub series: Option<String>,
    /// Set by `rollback`: the version the default boot entry is held on until
    /// a newer fix inside the series is applied, or the component is promoted
    /// or marked good again.
    pub boot_pin: Option<Evr>,
    /// Set by `rollback` on a component with no series to gate on (`follow`,
    /// `soak`, `hold`): nothing newer than this installs until `promote`
    /// releases it, or the next update would undo the rollback.
    pub held_at: Option<Evr>,
    pub gated: Option<GatedCandidate>,
    /// When each candidate version was first observed in the repo. This is what
    /// makes the `soak` policy possible, and it is why `check` must run on a
    /// timer even though it changes nothing else.
    pub first_seen: BTreeMap<String, DateTime<Utc>>,
    pub vault: Vec<VaultEntry>,
}

impl ComponentState {
    /// Record that `version` is available now, returning when it was first seen.
    pub fn observe(&mut self, version: &Evr, now: DateTime<Utc>) -> DateTime<Utc> {
        *self.first_seen.entry(version.to_string()).or_insert(now)
    }

    /// Record `version` as the gated candidate for `series`, keeping the
    /// notification timestamp if it is the same candidate as before.
    pub fn record_gated(&mut self, series: &str, version: &Evr, now: DateTime<Utc>) {
        let first_seen = self.observe(version, now);
        let notified_at = self
            .gated
            .as_ref()
            .filter(|g| &g.version == version)
            .and_then(|g| g.notified_at);
        self.gated = Some(GatedCandidate {
            series: series.to_string(),
            version: version.clone(),
            first_seen,
            notified_at,
        });
    }

    /// Forget first-seen entries for versions that are no longer offered, so the
    /// map does not grow without bound on a rolling distribution.
    pub fn retain_seen(&mut self, available: &[Evr]) {
        let keep: Vec<String> = available.iter().map(Evr::to_string).collect();
        self.first_seen.retain(|k, _| keep.contains(k));
    }

    pub fn is_vaulted(&self, version: &Evr) -> bool {
        self.vault.iter().any(|v| &v.version == version)
    }
}

/// One run of `sluice update`, kept so failures and pending reboots are seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRun {
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub ok: bool,
    /// Why it failed, in words, with zypper's last lines.
    pub error: Option<String>,
    /// What sluice applied itself, e.g. `kernel 7.2.6-1.1`.
    pub applied: Vec<String>,
    /// zypper's own one-line summary of the upgrade.
    pub summary: Option<String>,
}

/// What `migrate` replaced, so `migrate --undo` can restore it exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub at: DateTime<Utc>,
    pub prior_locks: Vec<LockSpec>,
    pub prior_multiversion: Option<String>,
    /// Component state as it was before `migrate` recorded anything.
    #[serde(default)]
    pub prior_components: BTreeMap<String, ComponentState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub components: BTreeMap<String, ComponentState>,
    pub migration: Option<MigrationRecord>,
    pub last_update: Option<DateTime<Utc>>,
    pub last_check: Option<DateTime<Utc>>,
    /// Versions sluice itself added to `multiversion.kernels`. Only these are
    /// ever unpinned; an explicit version you added by hand is left alone.
    pub pinned: Vec<Evr>,
    /// The newest sluice release already announced, so `check` says it once.
    pub announced_release: Option<String>,
    /// Recent update runs, newest last.
    pub update_runs: Vec<UpdateRun>,
    /// The failed run already announced (by its start time).
    pub announced_failure: Option<DateTime<Utc>>,
    /// The reboot reason already announced.
    pub announced_reboot: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            components: BTreeMap::new(),
            migration: None,
            last_update: None,
            last_check: None,
            pinned: Vec::new(),
            announced_release: None,
            update_runs: Vec::new(),
            announced_failure: None,
            announced_reboot: None,
        }
    }
}

impl State {
    /// Load state, treating a missing file as a fresh install.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let state: State = serde_json::from_str(&text)
                    .with_context(|| format!("parsing state file {}", path.display()))?;
                anyhow::ensure!(
                    state.version <= STATE_VERSION,
                    "state file {} was written by a newer sluice (format {}, this build understands {})",
                    path.display(),
                    state.version,
                    STATE_VERSION
                );
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading state file {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("state file path has no parent directory")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating state directory {}", dir.display()))?;

        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn record_update(&mut self, run: UpdateRun) {
        self.update_runs.push(run);
        let excess = self.update_runs.len().saturating_sub(30);
        self.update_runs.drain(..excess);
    }

    pub fn last_update_run(&self) -> Option<&UpdateRun> {
        self.update_runs.last()
    }

    pub fn last_successful_update(&self) -> Option<&UpdateRun> {
        self.update_runs.iter().rev().find(|r| r.ok)
    }

    pub fn component(&self, name: &str) -> ComponentState {
        self.components.get(name).cloned().unwrap_or_default()
    }

    pub fn component_mut(&mut self, name: &str) -> &mut ComponentState {
        self.components.entry(name.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_file_loads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::load(&dir.path().join("nope.json")).unwrap();
        assert!(state.components.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");

        let mut state = State::default();
        let c = state.component_mut("kernel");
        c.known_good = Some(Evr::parse("7.0.12-1.1"));
        c.lifecycle = Lifecycle::Testing;
        c.testing = Some(Evr::parse("7.2.6-1.1"));
        state.save(&path).unwrap();

        let back = State::load(&path).unwrap();
        let c = back.component("kernel");
        assert_eq!(c.known_good.unwrap().to_string(), "7.0.12-1.1");
        assert_eq!(c.lifecycle, Lifecycle::Testing);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file left behind"
        );
    }

    #[test]
    fn newer_state_format_is_refused_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version": 999, "components": {}}"#).unwrap();
        let err = State::load(&path).unwrap_err().to_string();
        assert!(err.contains("newer sluice"), "unexpected error: {err}");
    }

    #[test]
    fn first_seen_is_sticky_and_prunes_withdrawn_versions() {
        let t0 = "2026-09-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let t1 = "2026-09-08T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let v = Evr::parse("7.2.6-1.1");

        let mut c = ComponentState::default();
        assert_eq!(c.observe(&v, t0), t0);
        // Observing again must not reset the clock, or soak would never elapse.
        assert_eq!(c.observe(&v, t1), t0);

        c.observe(&Evr::parse("7.2.5-1.1"), t0);
        c.retain_seen(std::slice::from_ref(&v));
        assert_eq!(c.first_seen.len(), 1);
        assert!(c.first_seen.contains_key("7.2.6-1.1"));
    }
}
