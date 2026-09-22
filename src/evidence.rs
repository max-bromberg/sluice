//! What this machine has lived through: which versions were installed and
//! removed when, and which kernel each boot ran — and from that, how each
//! kernel has actually behaved here.
//!
//! The two best sources need root. zypp's history log is root-only, and a
//! boot's kernel is only named in the kernel's own journal messages. So the
//! runs that have root (the `check` timer, `update`, `health` under sudo)
//! save what they see to a small, world-readable evidence file in sluice's
//! state directory, and everything else reads that. Where neither says which
//! kernel a boot ran, it is inferred from the install history — sdbootutil
//! boots the newest installed kernel — and marked as inferred wherever it is
//! shown.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::health::{BootRecord, HealthReport};
use crate::version::Evr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Install,
    Remove,
}

/// One package transaction, from zypp's history log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub at: DateTime<Utc>,
    pub action: Action,
    pub name: String,
    pub evr: Evr,
}

/// Parse zypp's history log (`/var/log/zypp/history`), keeping only the
/// packages named. Lines look like
/// `2026-08-27 11:12:25|install|kernel-default|7.2.0-1.1|x86_64|user@host|repo|sha|`;
/// removals are `remove ` with a trailing space. Comments start with `#`.
/// Only the date, action, name and version are kept.
pub fn parse_zypp_history(text: &str, names: &BTreeSet<String>) -> Vec<HistoryEvent> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 4 {
                return None;
            }
            let action = match f[1].trim() {
                "install" => Action::Install,
                "remove" => Action::Remove,
                _ => return None,
            };
            let name = f[2].trim();
            if !names.contains(name) {
                return None;
            }
            let naive = NaiveDateTime::parse_from_str(f[0].trim(), "%Y-%m-%d %H:%M:%S").ok()?;
            let at = Local
                .from_local_datetime(&naive)
                .earliest()?
                .with_timezone(&Utc);
            Some(HistoryEvent {
                at,
                action,
                name: name.to_string(),
                evr: Evr::parse(f[3].trim()),
            })
        })
        .collect()
}

/// What root runs have seen, kept for everyone else.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub updated_at: Option<DateTime<Utc>>,
    /// Install and remove events for tracked packages.
    pub history: Vec<HistoryEvent>,
    /// Boot id → kernel release, as named in the boot's own journal.
    pub boot_kernels: BTreeMap<String, String>,
}

impl Evidence {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("evidence.json")
    }

    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(Self::path(state_dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = Self::path(state_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Fold in a fresh read of the history log and of the journal. History
    /// replaces what was kept (the log is the source of truth); boot kernels
    /// accumulate, since the journal rotates and forgets.
    pub fn merge(
        &mut self,
        history: Option<Vec<HistoryEvent>>,
        report: &HealthReport,
        now: DateTime<Utc>,
    ) {
        if let Some(h) = history {
            self.history = h;
        }
        for b in &report.boots {
            if let (Some(k), false) = (&b.kernel, b.kernel_inferred) {
                self.boot_kernels.insert(b.boot_id.clone(), k.clone());
            }
        }
        self.updated_at = Some(now);
    }
}

/// `7.2.0-1.1` of `kernel-default` runs as `7.2.0-1-default`.
pub fn kernel_release(anchor: &str, evr: &Evr) -> String {
    let flavor = anchor.strip_prefix("kernel-").unwrap_or("default");
    let rel_major = evr.release.split('.').next().unwrap_or("");
    format!("{}-{rel_major}-{flavor}", evr.version)
}

/// Fill in the kernel of every boot the journal did not name: first from what
/// root runs recorded, then by inference from the install history.
///
/// `installed_now` covers the case with no history at all: the versions
/// installed today and when, from the package database.
pub fn attribute(
    report: &mut HealthReport,
    evidence: &Evidence,
    anchor: &str,
    running: Option<&str>,
    installed_now: &[(Evr, Option<DateTime<Utc>>)],
) {
    // Periods during which each version was installed.
    let mut periods: Vec<(Evr, DateTime<Utc>, Option<DateTime<Utc>>)> = Vec::new();
    let mut events: Vec<&HistoryEvent> = evidence
        .history
        .iter()
        .filter(|e| e.name == anchor)
        .collect();
    events.sort_by_key(|e| e.at);
    for e in events {
        match e.action {
            Action::Install => periods.push((e.evr.clone(), e.at, None)),
            Action::Remove => {
                if let Some(p) = periods
                    .iter_mut()
                    .rev()
                    .find(|p| p.0 == e.evr && p.2.is_none())
                {
                    p.2 = Some(e.at);
                }
            }
        }
    }
    let have_history = !periods.is_empty();
    for (evr, at) in installed_now {
        if !periods.iter().any(|p| &p.0 == evr) {
            // Installed before any history we have; assume it always was.
            periods.push((evr.clone(), at.unwrap_or(DateTime::<Utc>::MIN_UTC), None));
        }
    }
    // Without the history log there is no record of kernels installed and
    // since removed. A boot before the newest install might have run one of
    // those, so only boots after it can be attributed safely — and to that
    // newest kernel. Guessing further back would pin old freezes on whichever
    // kernel happens to still be installed.
    let safe_from = if have_history {
        DateTime::<Utc>::MIN_UTC
    } else {
        installed_now
            .iter()
            .filter_map(|(_, at)| *at)
            .max()
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    };

    for b in &mut report.boots {
        if b.kernel.is_some() {
            continue;
        }
        if let Some(k) = evidence.boot_kernels.get(&b.boot_id) {
            b.kernel = Some(k.clone());
            continue;
        }
        if b.index == 0 {
            if let Some(k) = running {
                b.kernel = Some(k.to_string());
                continue;
            }
        }
        if b.start < safe_from {
            continue;
        }
        // The newest version installed when the boot started is what
        // sdbootutil made the default, so almost certainly what ran.
        let newest = periods
            .iter()
            .filter(|(_, from, to)| *from <= b.start && to.is_none_or(|t| t > b.start))
            .map(|(evr, _, _)| evr)
            .max();
        if let Some(evr) = newest {
            b.kernel = Some(kernel_release(anchor, evr));
            b.kernel_inferred = true;
        }
    }
}

/// How one kernel has behaved on this machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KernelRecord {
    pub boots: usize,
    pub hours: f64,
    pub unclean: usize,
    pub pstore: usize,
    /// Boots whose kernel was inferred rather than recorded.
    pub inferred: usize,
}

impl KernelRecord {
    /// Unclean ends per 100 hours of uptime: comparable across kernels that
    /// ran for very different lengths of time.
    pub fn rate(&self) -> Option<f64> {
        (self.hours >= 1.0).then(|| self.unclean as f64 / self.hours * 100.0)
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} boot{} · {:.0} h · {} unclean end{}",
            self.boots,
            if self.boots == 1 { "" } else { "s" },
            self.hours,
            self.unclean,
            if self.unclean == 1 { "" } else { "s" }
        );
        if let Some(r) = self.rate().filter(|_| self.unclean > 0) {
            s.push_str(&format!(" ({r:.1} per 100 h)"));
        }
        if self.pstore > 0 {
            s.push_str(&format!(" · {} crash record(s)", self.pstore));
        }
        if self.inferred > 0 {
            s.push_str(&format!(" · {} of the boots inferred", self.inferred));
        }
        s
    }
}

/// Group boots by the upstream version of their kernel (`7.2.0`).
pub fn per_version(boots: &[BootRecord]) -> BTreeMap<String, KernelRecord> {
    let mut out: BTreeMap<String, KernelRecord> = BTreeMap::new();
    for b in boots {
        let Some(k) = &b.kernel else { continue };
        let version = k.split('-').next().unwrap_or(k).to_string();
        let r = out.entry(version).or_default();
        r.boots += 1;
        r.hours += b.duration().num_minutes() as f64 / 60.0;
        r.unclean += usize::from(!b.clean_end);
        r.pstore += b.pstore_hits;
        r.inferred += usize::from(b.kernel_inferred);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn names() -> BTreeSet<String> {
        ["kernel-default".to_string()].into()
    }

    /// The zypp history format, including the trailing space on `remove `,
    /// comment lines and command records.
    #[test]
    fn parses_zypp_history() {
        let text = "\
# 2026-08-27 11:12:21 kernel-default-7.2.0-1.1.x86_64.rpm installed ok
2026-08-27 11:10:00|command|admin@host|'zypper' 'dup'|
2026-08-27 11:12:25|install|kernel-default|7.2.0-1.1|x86_64|admin@host|repo-oss|abc123|
2026-08-27 11:12:26|install|Mesa|26.2.1-1.1|x86_64|admin@host|repo-oss|def456|
2026-09-10 10:00:00|remove |kernel-default|6.18.4-1.1|x86_64|admin@host|
";
        let events = parse_zypp_history(text, &names());
        assert_eq!(
            events.len(),
            2,
            "untracked packages and commands are skipped"
        );
        assert_eq!(events[0].action, Action::Install);
        assert_eq!(events[0].evr.to_string(), "7.2.0-1.1");
        assert_eq!(events[1].action, Action::Remove);
        assert!(
            !format!("{events:?}").contains("host"),
            "nothing identifying is kept"
        );
    }

    fn boot(id: &str, start: DateTime<Utc>, hours: i64, clean: bool) -> BootRecord {
        BootRecord {
            index: -1,
            boot_id: id.into(),
            start,
            end: start + Duration::hours(hours),
            kernel: None,
            kernel_inferred: false,
            clean_end: clean,
            pstore_hits: 0,
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn attributes_boots_from_records_then_from_history() {
        let mut report = HealthReport {
            boots: vec![
                boot("a", at("2026-06-01T08:00:00Z"), 10, true),
                boot("b", at("2026-08-28T08:00:00Z"), 13, false),
                boot("c", at("2026-09-01T08:00:00Z"), 5, true),
            ],
            unavailable: None,
            kernels_unknown: None,
        };
        let evidence = Evidence {
            history: vec![
                HistoryEvent {
                    at: at("2026-05-30T08:00:00Z"),
                    action: Action::Install,
                    name: "kernel-default".into(),
                    evr: Evr::parse("7.0.12-1.1"),
                },
                HistoryEvent {
                    at: at("2026-08-27T08:00:00Z"),
                    action: Action::Install,
                    name: "kernel-default".into(),
                    evr: Evr::parse("7.2.0-1.1"),
                },
            ],
            boot_kernels: [("c".to_string(), "7.0.12-1-default".to_string())].into(),
            ..Default::default()
        };
        attribute(&mut report, &evidence, "kernel-default", None, &[]);

        assert_eq!(report.boots[0].kernel.as_deref(), Some("7.0.12-1-default"));
        assert!(report.boots[0].kernel_inferred);
        assert_eq!(
            report.boots[1].kernel.as_deref(),
            Some("7.2.0-1-default"),
            "the newest installed kernel boots"
        );
        assert_eq!(
            report.boots[2].kernel.as_deref(),
            Some("7.0.12-1-default"),
            "a recorded kernel beats inference"
        );
        assert!(!report.boots[2].kernel_inferred);

        let per = per_version(&report.boots);
        let v72 = &per["7.2.0"];
        assert_eq!((v72.boots, v72.unclean, v72.inferred), (1, 1, 1));
        assert!((v72.rate().unwrap() - 100.0 / 13.0).abs() < 0.01);
        assert!(v72.summary().contains("inferred"));
    }

    #[test]
    fn a_removed_kernel_stops_being_a_candidate() {
        let mut report = HealthReport {
            boots: vec![boot("x", at("2026-09-05T08:00:00Z"), 1, true)],
            unavailable: None,
            kernels_unknown: None,
        };
        let ev = |day: &str, action, v: &str| HistoryEvent {
            at: at(day),
            action,
            name: "kernel-default".into(),
            evr: Evr::parse(v),
        };
        let evidence = Evidence {
            history: vec![
                ev("2026-08-01T00:00:00Z", Action::Install, "7.0.12-1.1"),
                ev("2026-08-20T00:00:00Z", Action::Install, "7.1.2-1.1"),
                ev("2026-09-01T00:00:00Z", Action::Remove, "7.1.2-1.1"),
            ],
            ..Default::default()
        };
        attribute(&mut report, &evidence, "kernel-default", None, &[]);
        assert_eq!(report.boots[0].kernel.as_deref(), Some("7.0.12-1-default"));
    }

    /// With no history log, a boot before the newest install could have run a
    /// kernel that was since removed, so it stays unattributed.
    #[test]
    fn without_history_only_boots_after_the_newest_install_are_attributed() {
        let mut report = HealthReport {
            boots: vec![
                boot("old", at("2026-07-01T08:00:00Z"), 5, false),
                boot("new", at("2026-08-28T08:00:00Z"), 5, false),
            ],
            unavailable: None,
            kernels_unknown: None,
        };
        let installed = vec![
            (Evr::parse("7.0.12-1.1"), Some(at("2026-06-15T08:00:00Z"))),
            (Evr::parse("7.2.0-1.1"), Some(at("2026-08-27T08:00:00Z"))),
        ];
        attribute(
            &mut report,
            &Evidence::default(),
            "kernel-default",
            None,
            &installed,
        );
        assert_eq!(
            report.boots[0].kernel, None,
            "7.1.x may have run then; unknown is honest"
        );
        assert_eq!(report.boots[1].kernel.as_deref(), Some("7.2.0-1-default"));
    }

    #[test]
    fn evidence_round_trips_and_accumulates_boot_kernels() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = Evidence::default();
        let mut report = HealthReport {
            boots: vec![boot("a", at("2026-06-01T08:00:00Z"), 1, true)],
            unavailable: None,
            kernels_unknown: None,
        };
        report.boots[0].kernel = Some("7.2.0-1-default".into());
        e.merge(None, &report, at("2026-09-22T00:00:00Z"));
        e.save(dir.path()).unwrap();

        let mut back = Evidence::load(dir.path());
        assert_eq!(back.boot_kernels["a"], "7.2.0-1-default");
        // The journal later forgets boot "a"; the record is kept.
        report.boots.clear();
        back.merge(None, &report, at("2026-09-23T00:00:00Z"));
        assert!(back.boot_kernels.contains_key("a"));
    }
}
