//! Boot-health evidence.
//!
//! On this class of failure — a hard freeze that leaves nothing in the log —
//! there is no oops to find and no error to grep for. The only durable signal
//! is negative: a boot whose journal simply stops, with none of the shutdown
//! sequence that a deliberate reboot always writes. That absence, counted per
//! kernel version, is the evidence sluice offers. It never draws the
//! conclusion; marking a kernel good stays a human decision.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use regex::RegexSet;
use serde::{Deserialize, Serialize};

use crate::config::HealthConfig;
use crate::exec::{Cmd, Runner};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootRecord {
    /// journalctl's relative index: 0 is the current boot, -1 the previous.
    pub index: i32,
    pub boot_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// `None` when the boot's kernel messages are no longer retained.
    pub kernel: Option<String>,
    pub clean_end: bool,
    /// Crash-dump records archived within this boot's window.
    pub pstore_hits: usize,
}

impl BootRecord {
    pub fn duration(&self) -> Duration {
        self.end - self.start
    }

    pub fn is_current(&self) -> bool {
        self.index == 0
    }
}

/// Boot evidence aggregated per kernel version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KernelHealth {
    pub kernel: String,
    pub boots: usize,
    pub uptime_hours: f64,
    pub unclean_ends: usize,
    pub pstore_hits: usize,
}

impl KernelHealth {
    /// A one-line verdict for `status`. Phrased as evidence, not a judgement.
    pub fn summary(&self) -> String {
        let plural = if self.boots == 1 { "boot" } else { "boots" };
        let mut s = format!(
            "{} {plural}, {:.0} h uptime, {} unclean end{}",
            self.boots,
            self.uptime_hours,
            self.unclean_ends,
            if self.unclean_ends == 1 { "" } else { "s" }
        );
        if self.pstore_hits > 0 {
            s.push_str(&format!(", {} pstore record(s)", self.pstore_hits));
        }
        s
    }
}

/// `journalctl --list-boots -o json` entries. Timestamps are microseconds
/// since the epoch.
#[derive(Debug, Deserialize)]
struct RawBoot {
    index: i32,
    boot_id: String,
    first_entry: i64,
    last_entry: i64,
}

pub struct HealthReport {
    pub boots: Vec<BootRecord>,
    /// Present when the journal could not be read at all — a report that is
    /// merely empty is a different thing from one that could not be produced.
    pub unavailable: Option<String>,
    /// Present when boots could be scored but not attributed to a kernel,
    /// typically because only the user's own journal is readable.
    pub kernels_unknown: Option<String>,
}

impl HealthReport {
    pub fn by_kernel(&self) -> Vec<KernelHealth> {
        let mut map: BTreeMap<String, KernelHealth> = BTreeMap::new();
        for b in &self.boots {
            let key = b.kernel.clone().unwrap_or_else(|| "unknown".into());
            let e = map.entry(key.clone()).or_insert_with(|| KernelHealth {
                kernel: key,
                ..Default::default()
            });
            e.boots += 1;
            e.uptime_hours += b.duration().num_seconds() as f64 / 3600.0;
            if !b.clean_end {
                e.unclean_ends += 1;
            }
            e.pstore_hits += b.pstore_hits;
        }
        let mut out: Vec<_> = map.into_values().collect();
        out.sort_by(|a, b| b.uptime_hours.total_cmp(&a.uptime_hours));
        out
    }

    pub fn for_kernel(&self, kernel: &str) -> Option<KernelHealth> {
        self.by_kernel().into_iter().find(|k| k.kernel == kernel)
    }

    /// Unclean ends, newest first — what `check` notifies on.
    pub fn unclean(&self) -> Vec<&BootRecord> {
        let mut v: Vec<&BootRecord> = self.boots.iter().filter(|b| !b.clean_end).collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.end));
        v
    }
}

pub fn report(cfg: &HealthConfig, r: &mut Runner) -> Result<HealthReport> {
    let list = r.run(
        &Cmd::read(&cfg.journalctl)
            .arg("--list-boots")
            .arg("-o")
            .arg("json")
            .arg("--no-pager"),
    )?;
    if !list.ok() {
        return Ok(HealthReport {
            boots: Vec::new(),
            unavailable: Some(format!(
                "journalctl --list-boots failed: {}",
                list.stderr.trim()
            )),
            kernels_unknown: None,
        });
    }

    let raw = parse_boot_list(&list.stdout)?;
    let pstore = pstore_times(&cfg.pstore_dir);
    let matcher = RegexSet::new(&cfg.clean_end_patterns).context("compiling clean-end patterns")?;

    // The running kernel always logged its banner this boot. If we cannot see
    // it, kernel messages are not readable to us and asking per boot is futile.
    let kernels_readable = boot_kernel(cfg, r, "0")?.is_some();

    let mut boots = Vec::new();
    for b in raw {
        let start = micros_to_utc(b.first_entry);
        let end = micros_to_utc(b.last_entry);
        if (end - start).num_seconds() < cfg.min_boot_secs as i64 {
            continue;
        }

        // The current boot has not ended, so it is neither clean nor unclean.
        let clean_end = if b.index == 0 {
            true
        } else {
            boot_ended_cleanly(cfg, r, &b.boot_id, &matcher)?
        };

        boots.push(BootRecord {
            index: b.index,
            boot_id: b.boot_id.clone(),
            start,
            end,
            kernel: if kernels_readable {
                boot_kernel(cfg, r, &b.boot_id)?
            } else {
                None
            },
            clean_end,
            pstore_hits: pstore.iter().filter(|t| **t >= start && **t <= end).count(),
        });
    }

    boots.sort_by_key(|b| b.index);
    Ok(HealthReport {
        boots,
        unavailable: None,
        kernels_unknown: (!kernels_readable).then(|| {
            "kernel messages are not readable, so boots cannot be attributed to a kernel; \
             run as root or join the systemd-journal group"
                .to_string()
        }),
    })
}

fn parse_boot_list(json: &str) -> Result<Vec<RawBoot>> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).context("parsing journalctl --list-boots JSON")
}

fn micros_to_utc(micros: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(micros)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Look for the shutdown sequence in the tail of a boot's journal.
fn boot_ended_cleanly(
    cfg: &HealthConfig,
    r: &mut Runner,
    boot_id: &str,
    matcher: &RegexSet,
) -> Result<bool> {
    let out = r.run(
        &Cmd::read(&cfg.journalctl)
            .arg("-b")
            .arg(boot_id)
            .arg("-n")
            .arg(cfg.tail_lines.to_string())
            .arg("-o")
            .arg("cat")
            .arg("--no-pager"),
    )?;
    if !out.ok() {
        // A boot whose journal has been rotated away tells us nothing. Treating
        // it as unclean would invent evidence, so call it clean and move on.
        return Ok(true);
    }
    Ok(out.stdout.lines().any(|l| matcher.is_match(l)))
}

fn boot_kernel(cfg: &HealthConfig, r: &mut Runner, boot_id: &str) -> Result<Option<String>> {
    let out = r.run(
        &Cmd::read(&cfg.journalctl)
            .arg("-b")
            .arg(boot_id)
            .arg("-k")
            .arg("-g")
            .arg("Linux version [0-9]")
            .arg("-o")
            .arg("cat")
            .arg("--no-pager"),
    )?;
    Ok(out.ok().then(|| parse_kernel_line(&out.stdout)).flatten())
}

/// Pull the version out of `Linux version 7.2.0-1-default (geeko@buildhost) ...`.
pub fn parse_kernel_line(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.split_once("Linux version "))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map(str::to_string)
}

/// Modification times of everything under the pstore archive directory.
fn pstore_times(dir: &Path) -> Vec<DateTime<Utc>> {
    fn walk(dir: &Path, out: &mut Vec<DateTime<Utc>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(t) = entry.metadata().and_then(|m| m.modified()) {
                out.push(system_time_to_utc(t));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

fn system_time_to_utc(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Render a boot record's window in the local timezone, for display.
pub fn local_window(b: &BootRecord) -> String {
    let start: DateTime<Local> = b.start.into();
    let end: DateTime<Local> = b.end.into();
    format!(
        "{} → {}",
        start.format("%Y-%m-%d %H:%M"),
        end.format(if start.date_naive() == end.date_naive() {
            "%H:%M"
        } else {
            "%Y-%m-%d %H:%M"
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher() -> RegexSet {
        RegexSet::new(HealthConfig::default().clean_end_patterns).unwrap()
    }

    #[test]
    fn parses_list_boots_json() {
        let json = r#"[
          {"index":-1,"boot_id":"70de795c","first_entry":1758467424000000,"last_entry":1758514238000000},
          {"index":0,"boot_id":"87c19b69","first_entry":1758556673000000,"last_entry":1758559213000000}
        ]"#;
        let boots = parse_boot_list(json).unwrap();
        assert_eq!(boots.len(), 2);
        assert_eq!(boots[0].index, -1);
        assert_eq!(boots[1].boot_id, "87c19b69");
    }

    #[test]
    fn empty_boot_list_is_not_an_error() {
        assert!(parse_boot_list("").unwrap().is_empty());
    }

    /// The shape of a clean reboot's tail: the user manager winding down.
    #[test]
    fn recognises_a_clean_shutdown_tail() {
        let tail = "\
systemd[1234]: Stopped D-Bus User Message Bus.
systemd[1234]: Removed slice User Core Session Slice.
systemd[1234]: Reached target Shutdown.
systemd[1234]: Finished Exit the Session.
systemd[1234]: Reached target Exit the Session.";
        let m = matcher();
        assert!(tail.lines().any(|l| m.is_match(l)));
    }

    /// The shape of a hard freeze: ordinary desktop chatter, then nothing. No
    /// shutdown sequence appears anywhere in it.
    #[test]
    fn recognises_a_hard_freeze_tail() {
        let tail = "\
NetworkManager[812]: <info>  [1758500000.1234] dhcp4 (wlan0): state changed
kwin_wayland[2210]: kwin_core: XCB error: 3 (BadWindow)
plasmashell[2301]: qml: Notification applet: model reset
pipewire[2105]: spa.alsa: hw:1: snd_pcm_avail after recover: Broken pipe";
        let m = matcher();
        assert!(
            !tail.lines().any(|l| m.is_match(l)),
            "a frozen boot must not be scored as a clean shutdown"
        );
    }

    #[test]
    fn extracts_the_kernel_version() {
        let line = "Linux version 7.2.0-1-default (geeko@buildhost) (gcc (SUSE Linux) 15.1.0) #1 SMP PREEMPT_DYNAMIC";
        assert_eq!(parse_kernel_line(line).as_deref(), Some("7.2.0-1-default"));
        assert_eq!(parse_kernel_line("nothing here"), None);
    }

    fn boot(index: i32, kernel: &str, hours: i64, clean: bool) -> BootRecord {
        let start: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().unwrap();
        BootRecord {
            index,
            boot_id: format!("b{index}"),
            start,
            end: start + Duration::hours(hours),
            kernel: Some(kernel.into()),
            clean_end: clean,
            pstore_hits: 0,
        }
    }

    #[test]
    fn aggregates_evidence_per_kernel() {
        let report = HealthReport {
            boots: vec![
                boot(-3, "7.2.0-1-default", 13, false),
                boot(-2, "7.3.3-1-default", 70, true),
                boot(-1, "7.3.3-1-default", 71, true),
                boot(0, "7.3.3-1-default", 1, true),
            ],
            unavailable: None,
            kernels_unknown: None,
        };

        let by = report.by_kernel();
        let newest = by.iter().find(|k| k.kernel == "7.3.3-1-default").unwrap();
        assert_eq!(newest.boots, 3);
        assert_eq!(newest.unclean_ends, 0);
        assert_eq!(newest.uptime_hours.round(), 142.0);

        let old = report.for_kernel("7.2.0-1-default").unwrap();
        assert_eq!(old.unclean_ends, 1);
        assert!(old.summary().contains("1 unclean end"));

        assert_eq!(report.unclean().len(), 1);
    }
}
