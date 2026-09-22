//! The release timeline: every series of a tracked package laid out in time,
//! with where this machine sits on it.
//!
//! Upstream history comes from published indexes — kernel.org, the Mesa
//! archive, the linux-firmware tarball directory — and is fetched off the UI
//! thread. What this machine has (installed, running, default, known-good,
//! boots) is overlaid from local state. Releases still being developed are
//! projected from the cadence of the ones before them, and always marked as
//! projections.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use regex::Regex;

use crate::config::{ComponentConfig, LineageConfig, LineageSource};
use crate::lineage::{self, IndexEntry, Releases};
use crate::version::Evr;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseKind {
    /// The first release of a series (`7.2`, `26.2.0`).
    Mainline,
    /// A fix release inside a series.
    Point,
    /// A release candidate.
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub date: NaiveDate,
    pub kind: ReleaseKind,
    /// The date was inferred from cadence rather than published. Only used for
    /// kernel release candidates, which no index lists.
    pub inferred: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionKind {
    NextPoint,
    NextCandidate,
    NextMainline,
}

/// A release that has not happened yet, placed by the cadence of the ones
/// before it. Never shown as anything but a projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub label: String,
    pub date: NaiveDate,
    /// How uncertain the date is, in days either side.
    pub spread_days: i64,
    pub kind: ProjectionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    /// Still in release candidates.
    Development,
    Active,
    Longterm,
    /// Upstream has stopped releasing it.
    Eol,
}

/// One series (`7.2`, `26.2`), or all releases of a date-versioned package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub series: String,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    pub state: LaneState,
    pub releases: Vec<Release>,
    pub projections: Vec<Projection>,
}

impl Lane {
    pub fn first_date(&self) -> Option<NaiveDate> {
        self.start
            .into_iter()
            .chain(self.releases.iter().map(|r| r.date))
            .min()
    }

    pub fn last_date(&self) -> Option<NaiveDate> {
        self.end
            .into_iter()
            .chain(self.releases.iter().map(|r| r.date))
            .chain(self.projections.iter().map(|p| p.date))
            .max()
    }
}

/// Everything upstream says about a component, as fetched.
#[derive(Debug, Clone, Default)]
pub struct Upstream {
    pub lanes: Vec<Lane>,
    pub warnings: Vec<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub from_cache: bool,
}

/// What this machine has to do with one version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionMarks {
    pub installed: bool,
    pub installed_at: Option<NaiveDate>,
    /// The version the machine is running now.
    pub running: bool,
    /// The version the default boot entry starts.
    pub default_boot: bool,
    pub known_good: bool,
    pub testing: bool,
    /// Waiting at the gate.
    pub gated: bool,
    pub vaulted: bool,
    /// The version sluice considers the component to be on.
    pub current: bool,
    /// Offered by the repositories right now.
    pub offered: bool,
    /// Once installed here, since removed.
    pub removed_at: Option<NaiveDate>,
}

impl VersionMarks {
    pub fn any(&self) -> bool {
        *self != VersionMarks::default()
    }
}

/// One boot, for the machine lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSpan {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub clean: bool,
    pub kernel: Option<String>,
    pub kernel_inferred: bool,
    pub pstore_hits: usize,
}

impl BootSpan {
    /// The [`normalize`]d upstream version of the kernel it ran, if known.
    pub fn version(&self) -> Option<String> {
        self.kernel
            .as_deref()
            .map(|k| normalize(k.split('-').next().unwrap_or(k)))
    }
}

/// An install or removal of one version, from the history log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub at: DateTime<Utc>,
    pub removed: bool,
    /// [`normalize`]d upstream version.
    pub version: String,
}

/// This machine's side of the timeline.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    /// Keyed by [`normalize`]d upstream version.
    pub marks: BTreeMap<String, VersionMarks>,
    pub series: Option<String>,
    pub boots: Vec<BootSpan>,
    pub history: Vec<Change>,
    /// How each kernel version has behaved here, keyed like `marks`.
    pub records: BTreeMap<String, crate::evidence::KernelRecord>,
}

impl Machine {
    pub fn marks_for(&self, version: &str) -> VersionMarks {
        self.marks
            .get(&normalize(version))
            .cloned()
            .unwrap_or_default()
    }
}

/// kernel.org names a series' first release `7.2`; packages call it `7.2.0`.
/// Everything is compared in the three-component form.
pub fn normalize(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() == 2 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        format!("{version}.0")
    } else {
        version.to_string()
    }
}

// ---------------------------------------------------------------------------
// What a release changed, and what of it matters here
// ---------------------------------------------------------------------------

/// The measurable shape of one release, and the part of it that touches this
/// machine's hardware.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    pub patches: usize,
    pub reverts: usize,
    /// Counts for the component's configured highlight regexes.
    pub highlights: BTreeMap<String, usize>,
    /// Changes touching drivers this machine has loaded, by driver, hardware
    /// first.
    pub relevant: Vec<(String, usize, Tier)>,
    /// The most relevant change descriptions: bug titles for Mesa, commit
    /// subjects touching loaded drivers for the kernel.
    pub notable: Vec<String>,
    /// Every change touching this machine's drivers, with its ids, so later
    /// reverts can be matched to it.
    pub changes: Vec<ChangeRef>,
    /// Every revert in the release: what it undoes.
    pub reverts_of: Vec<RevertRef>,
}

/// A change to one of this machine's drivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRef {
    pub subject: String,
    pub driver: String,
    pub tier: Tier,
    /// Commit ids it is known by: its own and, for a stable backport, the
    /// upstream one.
    pub ids: Vec<String>,
}

/// A revert: the subject of what it undoes, and any commit ids it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertRef {
    pub subject: String,
    pub ids: Vec<String>,
}

impl RevertRef {
    /// Whether this revert undoes `change`: by commit id where both have one
    /// (ids are compared on their first 12 characters, the usual short form),
    /// else by subject.
    pub fn undoes(&self, change: &ChangeRef) -> bool {
        let short = |id: &str| id.chars().take(12).collect::<String>();
        let by_id = self
            .ids
            .iter()
            .any(|a| change.ids.iter().any(|b| short(a) == short(b)));
        by_id || same_subject(&self.subject, &change.subject)
    }
}

fn same_subject(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('.').to_ascii_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// `Revert "drm/amdgpu: foo"` → `drm/amdgpu: foo`.
fn reverted_subject(subject: &str) -> Option<String> {
    let rest = subject.trim().strip_prefix("Revert ")?;
    let rest = rest.trim();
    let inner = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(rest);
    Some(inner.to_string())
}

/// A change of this release that a later release reverted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reverted {
    pub subject: String,
    pub driver: String,
    /// The release that reverted it.
    pub by: String,
}

/// Per release of one series: its changes to this machine's drivers that
/// were reverted later, and the earlier ones it reverts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Watch {
    pub reverted_later: Vec<Reverted>,
    /// (subject, driver, release it came from)
    pub reverts_earlier: Vec<(String, String, String)>,
}

/// Match every revert in a series to the release whose change it undoes.
/// `releases` are (version, shape) oldest first; releases whose shape is not
/// known yet are skipped.
pub fn revert_watch(releases: &[(&str, &Shape)]) -> BTreeMap<String, Watch> {
    let mut out: BTreeMap<String, Watch> = BTreeMap::new();
    for (j, (later, later_shape)) in releases.iter().enumerate() {
        for revert in &later_shape.reverts_of {
            for (earlier, earlier_shape) in releases[..j].iter().rev() {
                if let Some(change) = earlier_shape.changes.iter().find(|c| revert.undoes(c)) {
                    out.entry((*earlier).to_string())
                        .or_default()
                        .reverted_later
                        .push(Reverted {
                            subject: change.subject.clone(),
                            driver: change.driver.clone(),
                            by: (*later).to_string(),
                        });
                    out.entry((*later).to_string())
                        .or_default()
                        .reverts_earlier
                        .push((
                            change.subject.clone(),
                            change.driver.clone(),
                            (*earlier).to_string(),
                        ));
                    break;
                }
            }
        }
    }
    out
}

/// How directly a driver concerns this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Drives a device here, supports one that does, or is a mounted filesystem.
    Hardware,
    /// Loaded, but not tied to this machine's hardware (netfilter, bridges…).
    Loaded,
}

/// What this machine runs, as far as changelogs are concerned: the drivers
/// bound to its devices, the modules those rely on, its filesystems, and
/// everything else loaded — plus the names changelogs use for them.
#[derive(Debug, Clone, Default)]
pub struct Relevance {
    /// Changelog token → (module, tier).
    tokens: BTreeMap<String, (String, Tier)>,
}

/// Modules nearly every machine loads, which would make every change look
/// relevant.
const CORE_MODULES: &[&str] = &[
    "drm",
    "drm_kms_helper",
    "drm_buddy",
    "drm_exec",
    "drm_display_helper",
    "drm_suballoc_helper",
    "drm_ttm_helper",
    "ttm",
    "snd",
    "snd_pcm",
    "snd_timer",
    "soundcore",
    "cfg80211",
    "mac80211",
    "bluetooth",
    "video",
    "i2c_core",
    "usbcore",
    "libata",
    "scsi_mod",
    "crc32c",
    "fuse",
    "button",
    "acpi",
    "wmi",
    "pcieportdrv",
    "hid_generic",
    "usbhid",
    "hid",
];

/// Suffixes too generic to stand for a driver on their own.
const GENERIC_WORDS: &[&str] = &[
    "core", "common", "generic", "platform", "acpi", "intel", "lib", "helper", "pci", "usb",
];

impl Relevance {
    /// Read from sysfs and procfs, which need no privileges.
    pub fn detect() -> Self {
        let proc_modules = std::fs::read_to_string("/proc/modules").unwrap_or_default();
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let bound = bound_modules();
        // Only filesystems on a real device; /proc/mounts also lists bpf,
        // cgroup2, tracefs and friends.
        let filesystems = mounts.lines().filter_map(|l| {
            let mut f = l.split_whitespace();
            let source = f.next()?;
            let fstype = f.nth(1)?;
            source.starts_with("/dev/").then_some(fstype)
        });
        Self::from_system(&proc_modules, &bound, filesystems)
    }

    /// `proc_modules` is `/proc/modules` text: name, size, refcount, users.
    pub fn from_system<'a>(
        proc_modules: &str,
        bound: &BTreeSet<String>,
        filesystems: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut loaded = Vec::new();
        for line in proc_modules.lines() {
            let mut f = line.split_whitespace();
            let Some(name) = f.next() else { continue };
            loaded.push(name.to_string());
            let used_by = f.nth(2).unwrap_or("-");
            users.insert(
                name.to_string(),
                used_by
                    .split(',')
                    .filter(|u| !u.is_empty() && *u != "-" && *u != "[permanent]")
                    .map(str::to_string)
                    .collect(),
            );
        }

        // Hardware: bound drivers, then whatever they rely on, transitively.
        let mut hardware: BTreeSet<String> = bound.clone();
        loop {
            let more: Vec<String> = users
                .iter()
                .filter(|(m, us)| !hardware.contains(*m) && us.iter().any(|u| hardware.contains(u)))
                .map(|(m, _)| m.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            hardware.extend(more);
        }
        hardware.extend(
            filesystems
                .into_iter()
                .map(|f| f.split('.').next().unwrap_or(f).to_string()),
        );

        let mut r = Relevance::default();
        for m in hardware.iter().chain(loaded.iter()) {
            let tier = if hardware.contains(m) {
                Tier::Hardware
            } else {
                Tier::Loaded
            };
            r.add(m, tier);
        }
        r
    }

    /// Every loaded module counts as hardware. For tests and simple callers.
    pub fn from_modules<'a>(modules: impl IntoIterator<Item = &'a str>) -> Self {
        let mut r = Relevance::default();
        for m in modules {
            r.add(m, Tier::Hardware);
        }
        r
    }

    fn add(&mut self, module: &str, tier: Tier) {
        if CORE_MODULES.contains(&module) {
            return;
        }
        let mut insert = |token: &str| {
            let entry = self
                .tokens
                .entry(token.to_ascii_lowercase())
                .or_insert_with(|| (module.to_string(), tier));
            if tier < entry.1 {
                *entry = (module.to_string(), tier);
            }
        };
        insert(module);
        insert(&module.replace('_', "-"));
        // `snd_hda_codec_realtek` is `realtek` in a changelog, `hid_apple` is
        // `apple`, `i2c_piix4` is `piix4`.
        // Only for families where that is the convention: `gpu_sched` is not
        // "sched", which is the network scheduler.
        const FAMILIES: &[&str] = &[
            "snd_", "hid_", "i2c_", "gpio_", "pinctrl_", "rtc_", "leds_", "spi_",
        ];
        if FAMILIES.iter().any(|f| module.starts_with(f)) {
            if let Some(last) = module.rsplit('_').next().filter(|l| *l != module) {
                if last.len() >= 4 && !GENERIC_WORDS.contains(&last) {
                    insert(last);
                }
            }
        }
        // `mt7925e` is `mt7925` in a changelog.
        if module.len() > 4 && module.ends_with(|c: char| c.is_ascii_alphabetic()) {
            let stem = &module[..module.len() - 1];
            if stem.ends_with(|c: char| c.is_ascii_digit()) {
                insert(stem);
            }
        }
        let aliases: &[&str] = match module {
            "amdgpu" => &[
                "drm/amd",
                "drm/amdgpu",
                "drm/amdkfd",
                "amdkfd",
                "radv",
                "radeonsi",
                "aco",
                "vcn",
            ],
            "i915" | "xe" => &["i915", "xe", "anv", "iris", "hasvk", "crocus"],
            "nouveau" => &["nvk", "nv50", "nvc0"],
            "kvm_amd" | "kvm_intel" => &["kvm", "svm", "vmx"],
            "iwlwifi" | "iwlmvm" => &["iwlwifi", "iwlmvm", "iwl"],
            "nvme" | "nvme_core" => &["nvme", "nvme-pci"],
            "snd_hda_intel" => &["hda"],
            "gpu_sched" => &["drm/sched", "drm/scheduler"],
            _ => &[],
        };
        for a in aliases {
            insert(a);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The driver a change description is about, if this machine loads it.
    /// Kernel subjects lead with a path (`drm/amd/display: …`, `wifi: mt76:
    /// mt7925: …`); Mesa ones with a driver (`radv: …`) or `[radv]` tags. The
    /// most specific segment wins.
    pub fn driver_of(&self, subject: &str) -> Option<&str> {
        self.classify(subject).map(|(m, _)| m)
    }

    pub fn classify(&self, subject: &str) -> Option<(&str, Tier)> {
        let s = subject.trim();
        let mut candidates: Vec<&str> = Vec::new();
        for segment in s.split(": ").take_while(|seg| !seg.contains(' ')).take(4) {
            candidates.extend(segment.split(['/', ',']));
            // Path prefixes, shortest first: `drm/amd`, `drm/amd/display`.
            let mut end = 0;
            while let Some(i) = segment[end..].find('/').map(|i| end + i) {
                if end > 0 {
                    candidates.push(&segment[..end - 1]);
                }
                end = i + 1;
            }
            if end > 0 {
                candidates.push(&segment[..end - 1]);
            }
            candidates.push(segment);
        }
        for tag in s.split('[').skip(1).filter_map(|t| t.split(']').next()) {
            candidates.extend(tag.split('/'));
        }
        for c in candidates.into_iter().rev() {
            if let Some((m, t)) = self.tokens.get(&c.to_ascii_lowercase()) {
                return Some((m.as_str(), *t));
            }
            // The most specific part names a chip (`mt7996`, `gfx12`) this
            // machine does not have: the change is about that chip, even if a
            // shared library (`mt76`) appears before it.
            let chip_like = c.chars().any(|ch| ch.is_ascii_digit())
                && c.chars().any(|ch| ch.is_ascii_alphabetic())
                && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if chip_like {
                return None;
            }
        }
        None
    }

    fn tally<'a>(
        &self,
        subjects: impl IntoIterator<Item = &'a str>,
        limit: usize,
    ) -> (Vec<(String, usize, Tier)>, Vec<String>) {
        let mut counts: BTreeMap<(Tier, String), usize> = BTreeMap::new();
        let mut hits: Vec<(Tier, String, &str)> = Vec::new();
        for subject in subjects {
            if let Some((driver, tier)) = self.classify(subject) {
                *counts.entry((tier, driver.to_string())).or_default() += 1;
                hits.push((tier, driver.to_string(), subject.trim()));
            }
        }
        // Hardware first; within a tier, the drivers with the most changes.
        let mut ranked: Vec<((Tier, String), usize)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| {
            a.0 .0
                .cmp(&b.0 .0)
                .then(b.1.cmp(&a.1))
                .then(a.0 .1.cmp(&b.0 .1))
        });
        let rank = |t: Tier, d: &str| ranked.iter().position(|((rt, rd), _)| *rt == t && rd == d);
        hits.sort_by_key(|(t, d, _)| rank(*t, d));
        let notable = hits
            .into_iter()
            .take(limit)
            .map(|(_, _, s)| s.to_string())
            .collect();
        let relevant = ranked.into_iter().map(|((t, d), n)| (d, n, t)).collect();
        (relevant, notable)
    }

    /// The tier of a driver, as named in [`Shape::relevant`].
    pub fn tier_of(&self, module: &str) -> Option<Tier> {
        self.tokens.get(module).map(|(_, t)| *t)
    }
}

/// Kernel modules whose drivers are bound to a device on this machine, read
/// from sysfs: the machine's actual hardware.
pub fn bound_modules() -> BTreeSet<String> {
    let mut bound: BTreeSet<String> = BTreeSet::new();
    for bus in [
        "pci", "usb", "platform", "hid", "i2c", "virtio", "nvme", "scsi",
    ] {
        let Ok(drivers) = std::fs::read_dir(format!("/sys/bus/{bus}/drivers")) else {
            continue;
        };
        for drv in drivers.flatten() {
            let path = drv.path();
            let has_device = std::fs::read_dir(&path).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(':'))
            });
            if !has_device {
                continue;
            }
            if let Ok(module) = std::fs::read_link(path.join("module")) {
                if let Some(name) = module.file_name() {
                    bound.insert(name.to_string_lossy().into_owned());
                }
            }
        }
    }
    bound
}

/// Shape a kernel.org ChangeLog, which is `git log` output of the stable
/// branch: each commit's subject, its upstream id, and what it reverts.
pub fn kernel_shape(
    text: &str,
    highlights: &BTreeMap<String, Regex>,
    relevance: &Relevance,
) -> Shape {
    let base = lineage::analyse_changelog(text, highlights);
    let header = Regex::new(r"^commit ([0-9a-f]{7,40})").expect("static regex");
    let upstream =
        Regex::new(r"(?:\[ Upstream commit ([0-9a-f]{7,40}) \]|commit ([0-9a-f]{7,40}) upstream)")
            .expect("static regex");
    let reverts_id = Regex::new(r"This reverts commit ([0-9a-f]{7,40})").expect("static regex");

    struct Commit<'a> {
        id: String,
        subject: Option<&'a str>,
        body: Vec<&'a str>,
    }
    let mut commits: Vec<Commit> = Vec::new();
    for line in text.lines() {
        if let Some(c) = header.captures(line) {
            commits.push(Commit {
                id: c[1].to_string(),
                subject: None,
                body: Vec::new(),
            });
        } else if let Some(c) = commits.last_mut() {
            if line.starts_with("    ") && !line.trim().is_empty() {
                if c.subject.is_none() {
                    c.subject = Some(line.trim());
                } else {
                    c.body.push(line.trim());
                }
            }
        }
    }

    let mut changes = Vec::new();
    let mut reverts = Vec::new();
    for c in &commits {
        let Some(subject) = c.subject else { continue };
        let mut ids = vec![c.id.clone()];
        for line in &c.body {
            if let Some(m) = upstream.captures(line) {
                ids.extend(m.get(1).or(m.get(2)).map(|x| x.as_str().to_string()));
            }
        }
        if let Some(inner) = reverted_subject(subject) {
            let named: Vec<String> = c
                .body
                .iter()
                .filter_map(|l| reverts_id.captures(l).map(|m| m[1].to_string()))
                .collect();
            reverts.push(RevertRef {
                subject: inner,
                ids: named,
            });
        } else if let Some((driver, tier @ Tier::Hardware)) = relevance.classify(subject) {
            // Only this machine's own drivers are watched for reverts.
            changes.push(ChangeRef {
                subject: subject.to_string(),
                driver: driver.to_string(),
                tier,
                ids,
            });
        }
    }

    let subjects: Vec<&str> = commits.iter().filter_map(|c| c.subject).collect();
    let (relevant, notable) = relevance.tally(subjects.iter().copied(), 40);
    Shape {
        patches: base.patches,
        reverts: base.reverts,
        highlights: base.highlights,
        relevant,
        notable,
        changes,
        reverts_of: reverts,
    }
}

/// Shape a Mesa release-notes page: its bug-fix titles are the notable
/// changes, its change list the patch count.
pub fn mesa_shape(
    html: &str,
    highlights: &BTreeMap<String, Regex>,
    relevance: &Relevance,
) -> Shape {
    let section = |id: &str| -> String {
        let start = html
            .find(&format!("<section id=\"{id}\""))
            .unwrap_or(html.len());
        let rest = &html[start..];
        let end = rest[1..].find("<section").map_or(rest.len(), |i| i + 1);
        rest[..end].to_string()
    };
    let items = |fragment: &str| -> Vec<String> {
        let li = Regex::new(r"(?s)<li><p>(.*?)</p>").expect("static regex");
        li.captures_iter(fragment)
            .map(|c| strip_tags(&c[1]))
            .filter(|t| !t.is_empty() && t != "None")
            .collect()
    };
    let bugs = items(&section("bug-fixes"));
    // Changes are grouped under author names, which end with a colon.
    let changes: Vec<String> = items(&section("changes"))
        .into_iter()
        .filter(|c| !c.ends_with(':'))
        .collect();

    let reverts = changes.iter().filter(|c| c.starts_with("Revert ")).count();
    let highlight_counts = highlights
        .iter()
        .map(|(label, re)| {
            (
                label.clone(),
                changes.iter().filter(|c| re.is_match(c)).count(),
            )
        })
        .collect();
    let (relevant, _) = relevance.tally(changes.iter().map(String::as_str), 0);
    // Bug titles read better than commit subjects; the ones naming this
    // machine's drivers go first.
    let (mut notable, rest): (Vec<String>, Vec<String>) = bugs
        .into_iter()
        .partition(|b| relevance.driver_of(b).is_some() || bug_mentions(b, relevance));
    notable.extend(rest);
    notable.truncate(40);

    let change_refs = changes
        .iter()
        .filter(|c| !c.starts_with("Revert "))
        .filter_map(|c| {
            relevance
                .classify(c)
                .filter(|(_, t)| *t == Tier::Hardware)
                .map(|(driver, tier)| ChangeRef {
                    subject: c.clone(),
                    driver: driver.to_string(),
                    tier,
                    ids: Vec::new(),
                })
        })
        .collect();
    let revert_refs = changes
        .iter()
        .filter_map(|c| reverted_subject(c))
        .map(|subject| RevertRef {
            subject,
            ids: Vec::new(),
        })
        .collect();

    Shape {
        patches: changes.len(),
        reverts,
        highlights: highlight_counts,
        relevant,
        notable,
        changes: change_refs,
        reverts_of: revert_refs,
    }
}

fn bug_mentions(title: &str, relevance: &Relevance) -> bool {
    let lower = title.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w.len() > 3 && relevance.tokens.contains_key(w))
}

fn strip_tags(s: &str) -> String {
    let tags = Regex::new(r"<[^>]+>").expect("static regex");
    let text = tags.replace_all(s, "");
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Building lanes
// ---------------------------------------------------------------------------

/// A published directory index of release artifacts, and how to read it.
#[derive(Debug, Clone)]
pub struct IndexSource {
    pub url: String,
    /// Capture group 1 is the version.
    pub file_pattern: String,
    /// Release notes for one version, with `{version}` substituted.
    pub notes_url: Option<String>,
}

/// The built-in index sources.
pub fn index_source(cfg: &ComponentConfig) -> Option<IndexSource> {
    match cfg.lineage {
        LineageSource::Mesa => Some(IndexSource {
            url: cfg
                .lineage_url
                .clone()
                .unwrap_or_else(|| "https://archive.mesa3d.org/".into()),
            file_pattern: cfg
                .lineage_pattern
                .clone()
                .unwrap_or_else(|| r"mesa-(\d+\.\d+\.\d+(?:-rc\d+)?)\.tar\.xz".into()),
            notes_url: Some("https://docs.mesa3d.org/relnotes/{version}.html".into()),
        }),
        LineageSource::LinuxFirmware => Some(IndexSource {
            url: cfg
                .lineage_url
                .clone()
                .unwrap_or_else(|| "https://cdn.kernel.org/pub/linux/kernel/firmware/".into()),
            file_pattern: cfg
                .lineage_pattern
                .clone()
                .unwrap_or_else(|| r"linux-firmware-(\d{8})\.tar\.xz".into()),
            notes_url: None,
        }),
        LineageSource::Index => Some(IndexSource {
            url: cfg.lineage_url.clone()?,
            file_pattern: cfg.lineage_pattern.clone()?,
            notes_url: None,
        }),
        LineageSource::None | LineageSource::LinuxStable => None,
    }
}

/// Parse any web server directory listing: each line linking a file that
/// matches `file_pattern`, with the date on that line. Handles nginx
/// (`17-Aug-2026 04:30`) and Apache (`2026-08-17 04:30`) listings.
pub fn parse_file_index(html: &str, file_pattern: &str) -> Vec<IndexEntry> {
    let Ok(file) = Regex::new(&format!(r#"href="{file_pattern}""#)) else {
        return Vec::new();
    };
    let date = Regex::new(r"(\d{2}-[A-Za-z]{3}-\d{4}|\d{4}-\d{2}-\d{2})").expect("static regex");
    let mut out: Vec<IndexEntry> = Vec::new();
    for line in html.lines() {
        let Some(c) = file.captures(line) else {
            continue;
        };
        let after = &line[c.get(0).map_or(0, |m| m.end())..];
        let Some(d) = date.find(after) else { continue };
        let parsed = NaiveDate::parse_from_str(d.as_str(), "%d-%b-%Y")
            .or_else(|_| NaiveDate::parse_from_str(d.as_str(), "%Y-%m-%d"));
        if let Ok(parsed) = parsed {
            let version = c[1].to_string();
            if !out.iter().any(|e| e.version == version) {
                out.push(IndexEntry {
                    version,
                    date: parsed,
                });
            }
        }
    }
    out.sort_by_key(|e| e.date);
    out
}

fn median_gap(dates: &[NaiveDate]) -> Option<i64> {
    let mut gaps: Vec<i64> = dates
        .windows(2)
        .map(|w| (w[1] - w[0]).num_days())
        .filter(|g| *g > 0)
        .collect();
    let recent = gaps.len().saturating_sub(6);
    let mut gaps = gaps.split_off(recent);
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2])
}

/// The next point release of an active lane, placed by its recent cadence.
fn project_next_point(lane: &Lane, today: NaiveDate) -> Option<Projection> {
    let points: Vec<&Release> = lane
        .releases
        .iter()
        .filter(|r| r.kind != ReleaseKind::Candidate)
        .collect();
    let last = points.last()?;
    let dates: Vec<NaiveDate> = points.iter().map(|r| r.date).collect();
    let gap = median_gap(&dates).unwrap_or(7).clamp(3, 60);
    let mut date = last.date + Duration::days(gap);
    // A release that is late is still coming; it is just late.
    if date < today {
        date = today + Duration::days(1);
    }
    let label = next_version(&normalize(&last.version)).unwrap_or_else(|| "next".into());
    Some(Projection {
        label,
        date,
        spread_days: (gap / 3).max(2),
        kind: ProjectionKind::NextPoint,
    })
}

/// `7.2.7` → `7.2.8`; `20260916` stays date-shaped and gets no label change.
fn next_version(v: &str) -> Option<String> {
    let (head, tail) = v.rsplit_once('.')?;
    let n: u64 = tail.parse().ok()?;
    Some(format!("{head}.{}", n + 1))
}

/// Lanes for the Linux kernel, from `releases.json` and the `vN.x/` indexes.
///
/// `keep` names series that must appear whatever their age: the ones this
/// machine has installed or is holding.
pub fn kernel_lanes(
    releases: &Releases,
    index: &[IndexEntry],
    re: &Regex,
    keep: &BTreeSet<String>,
    today: NaiveDate,
) -> Vec<Lane> {
    let mut lanes: Vec<Lane> = Vec::new();

    // The series in development: releases.json lists its newest candidate.
    if let Some(dev) = releases
        .releases
        .iter()
        .find(|r| r.moniker == "mainline" && r.version.contains("-rc"))
    {
        if let (Some(series), Some(date)) = (dev.series(re), dev.date()) {
            let n: i64 = dev
                .version
                .rsplit("-rc")
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(1);
            let mut rcs: Vec<Release> = (1..n)
                .map(|k| Release {
                    version: format!("{series}-rc{k}"),
                    date: date - Duration::weeks(n - k),
                    kind: ReleaseKind::Candidate,
                    inferred: true,
                })
                .collect();
            rcs.push(Release {
                version: dev.version.clone(),
                date,
                kind: ReleaseKind::Candidate,
                inferred: false,
            });
            let previous = Evr::parse(&series);
            let start = index
                .iter()
                .filter(|e| {
                    e.version.matches('.').count() == 1 && Evr::parse(&e.version) < previous
                })
                .map(|e| e.date)
                .max();
            // Mainline usually ships a week after rc7, sometimes after rc8.
            let final_date = date + Duration::weeks((8 - n).max(1));
            let mut projections = Vec::new();
            if n < 8 {
                projections.push(Projection {
                    label: format!("{series}-rc{}", n + 1),
                    date: date + Duration::weeks(1),
                    spread_days: 1,
                    kind: ProjectionKind::NextCandidate,
                });
            }
            projections.push(Projection {
                label: series.clone(),
                date: final_date.max(today + Duration::days(1)),
                spread_days: 7,
                kind: ProjectionKind::NextMainline,
            });
            lanes.push(Lane {
                series,
                start,
                end: None,
                state: LaneState::Development,
                releases: rcs,
                projections,
            });
        }
    }

    // Released series: those upstream lists, those this machine cares about,
    // and the most recent few from the index for context.
    let mut wanted: BTreeSet<String> = keep.clone();
    for r in &releases.releases {
        if r.moniker == "stable" {
            wanted.extend(r.series(re));
        }
    }
    let mut mainlines: Vec<&IndexEntry> = index
        .iter()
        .filter(|e| e.version.matches('.').count() == 1)
        .collect();
    mainlines.sort_by(|a, b| Evr::parse(&b.version).cmp(&Evr::parse(&a.version)));
    wanted.extend(mainlines.iter().take(4).map(|e| e.version.clone()));

    for series in wanted {
        if lanes.iter().any(|l| l.series == series) {
            continue;
        }
        let status = lineage::series_status(releases, index, &series, re);
        let mut rels: Vec<Release> = Vec::new();
        if let Some(m) = index.iter().find(|e| e.version == series) {
            rels.push(Release {
                version: series.clone(),
                date: m.date,
                kind: ReleaseKind::Mainline,
                inferred: false,
            });
        }
        rels.extend(
            lineage::points_of(index, &series)
                .into_iter()
                .map(|e| Release {
                    version: e.version.clone(),
                    date: e.date,
                    kind: ReleaseKind::Point,
                    inferred: false,
                }),
        );
        if rels.is_empty() {
            continue;
        }
        let state = match &status {
            Some(s) if s.eol => LaneState::Eol,
            Some(s) if s.is_longterm() => LaneState::Longterm,
            _ => LaneState::Active,
        };
        let mut lane = Lane {
            series: series.clone(),
            start: rels.first().map(|r| r.date),
            end: status.as_ref().filter(|s| s.eol).and_then(|s| s.eol_since),
            state,
            releases: rels,
            projections: Vec::new(),
        };
        if matches!(lane.state, LaneState::Active | LaneState::Longterm) {
            lane.projections.extend(project_next_point(&lane, today));
        }
        lanes.push(lane);
    }

    lanes.sort_by(|a, b| Evr::parse(&b.series).cmp(&Evr::parse(&a.series)));
    lanes
}

/// Lanes for anything published as a directory of release tarballs.
///
/// Versions the series regex cannot split (date-versioned firmware) share one
/// lane named after nothing in particular.
pub fn index_lanes(
    entries: &[IndexEntry],
    re: &Regex,
    keep: &BTreeSet<String>,
    today: NaiveDate,
) -> Vec<Lane> {
    let mut by_series: BTreeMap<String, Vec<Release>> = BTreeMap::new();
    for e in entries {
        let series = re
            .captures(&e.version)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let kind = if e.version.contains("-rc") {
            ReleaseKind::Candidate
        } else if !series.is_empty() && normalize(&series) == e.version {
            ReleaseKind::Mainline
        } else {
            ReleaseKind::Point
        };
        by_series.entry(series).or_default().push(Release {
            version: e.version.clone(),
            date: e.date,
            kind,
            inferred: false,
        });
    }

    let mut series_order: Vec<String> = by_series.keys().cloned().collect();
    series_order.sort_by(|a, b| Evr::parse(b).cmp(&Evr::parse(a)));
    let released: Vec<&String> = series_order
        .iter()
        .filter(|s| {
            by_series[*s]
                .iter()
                .any(|r| r.kind != ReleaseKind::Candidate)
        })
        .collect();
    let newest_released = released.first().map(|s| (*s).clone());
    let second_released = released.get(1).map(|s| (*s).clone());

    let mut lanes = Vec::new();
    for (i, series) in series_order.iter().enumerate() {
        if i >= 6 && !keep.contains(series) {
            continue;
        }
        let mut releases = by_series[series].clone();
        releases.sort_by_key(|r| r.date);
        let released = releases.iter().any(|r| r.kind != ReleaseKind::Candidate);
        let last = releases.last().map(|r| r.date);

        // The newest released series is active, and so is the one before it
        // while it still receives releases.
        let still_maintained = Some(series) == second_released.as_ref()
            && last.is_some_and(|d| (today - d).num_days() < 45);
        let state = if !released {
            LaneState::Development
        } else if Some(series) == newest_released.as_ref() || series.is_empty() || still_maintained
        {
            LaneState::Active
        } else {
            LaneState::Eol
        };

        let mut lane = Lane {
            series: series.clone(),
            start: releases.first().map(|r| r.date),
            end: (state == LaneState::Eol).then_some(last).flatten(),
            state,
            releases,
            projections: Vec::new(),
        };
        match lane.state {
            LaneState::Active if series.is_empty() => {
                // Date-versioned: the next one gets a date for a name.
                if let Some(mut p) = project_next_point(&lane, today) {
                    p.label = "next".into();
                    lane.projections.push(p);
                }
            }
            LaneState::Active => lane.projections.extend(project_next_point(&lane, today)),
            LaneState::Development => {
                let rcs = lane.releases.len() as i64;
                if let Some(last) = lane.releases.last() {
                    lane.projections.push(Projection {
                        label: normalize(series),
                        date: (last.date + Duration::weeks((4 - rcs).max(1)))
                            .max(today + Duration::days(1)),
                        spread_days: 7,
                        kind: ProjectionKind::NextMainline,
                    });
                }
            }
            _ => {}
        }
        lanes.push(lane);
    }
    lanes
}

// ---------------------------------------------------------------------------
// Fetching (runs off the UI thread)
// ---------------------------------------------------------------------------

/// Fetch and assemble a component's upstream lanes. Blocking; call it from a
/// worker thread.
pub fn fetch_upstream(
    lineage_cfg: &LineageConfig,
    cache_dir: &Path,
    cfg: &ComponentConfig,
    keep: &BTreeSet<String>,
    today: NaiveDate,
) -> Upstream {
    let mut fetcher = lineage::Fetcher::new(lineage_cfg, cache_dir);
    let Ok(re) = Regex::new(&cfg.series_regex) else {
        return Upstream {
            warnings: vec!["invalid series_regex".into()],
            ..Default::default()
        };
    };

    let lanes = match cfg.lineage {
        LineageSource::LinuxStable => {
            let releases = fetcher.releases().unwrap_or_default();
            let mut majors: BTreeSet<String> = releases
                .releases
                .iter()
                .filter(|r| r.moniker == "stable" || r.moniker == "mainline")
                .filter_map(|r| r.version.split('.').next().map(str::to_string))
                .collect();
            majors.extend(
                keep.iter()
                    .filter_map(|s| s.split('.').next().map(str::to_string)),
            );
            let mut index = Vec::new();
            for major in majors {
                index.extend(fetcher.index(&major).unwrap_or_default());
            }
            kernel_lanes(&releases, &index, &re, keep, today)
        }
        LineageSource::None => Vec::new(),
        _ => match index_source(cfg) {
            Some(src) => {
                let entries = fetcher
                    .get(&src.url)
                    .map(|html| parse_file_index(&html, &src.file_pattern))
                    .unwrap_or_default();
                if entries.is_empty() {
                    fetcher
                        .warnings
                        .push(format!("{} listed no matching releases", src.url));
                }
                index_lanes(&entries, &re, keep, today)
            }
            None => {
                fetcher
                    .warnings
                    .push("`lineage = \"index\"` needs lineage_url and lineage_pattern".into());
                Vec::new()
            }
        },
    };

    Upstream {
        lanes,
        warnings: fetcher.warnings,
        fetched_at: fetcher.oldest_fetch,
        from_cache: fetcher.used_cache,
    }
}

/// Fetch and shape one release's changes. Blocking.
pub fn fetch_shape(
    lineage_cfg: &LineageConfig,
    cache_dir: &Path,
    cfg: &ComponentConfig,
    version: &str,
    relevance: &Relevance,
) -> Option<Shape> {
    let highlights: BTreeMap<String, Regex> = cfg
        .highlight
        .iter()
        .filter_map(|(k, v)| Regex::new(v).ok().map(|r| (k.clone(), r)))
        .collect();
    let mut fetcher = lineage::Fetcher::new(lineage_cfg, cache_dir);
    match cfg.lineage {
        // A mainline ChangeLog is the whole merge window; not a useful shape.
        LineageSource::LinuxStable if version.matches('.').count() >= 2 => fetcher
            .changelog(version)
            .map(|text| kernel_shape(&text, &highlights, relevance)),
        LineageSource::Mesa => {
            let url = index_source(cfg)?.notes_url?.replace("{version}", version);
            fetcher
                .get_immutable(&url)
                .map(|html| mesa_shape(&html, &highlights, relevance))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn re() -> Regex {
        Regex::new(r"^(\d+\.\d+)").unwrap()
    }

    #[test]
    fn versions_compare_in_three_component_form() {
        assert_eq!(normalize("7.2"), "7.2.0");
        assert_eq!(normalize("7.2.6"), "7.2.6");
        assert_eq!(normalize("26.2.0"), "26.2.0");
        assert_eq!(normalize("20260916"), "20260916");
        assert_eq!(normalize("7.3-rc4"), "7.3-rc4");
    }

    /// Apache-style listing, as the Mesa archive serves it.
    #[test]
    fn parses_an_apache_index() {
        let html = r#"<tr><td><a href="mesa-26.2.2.tar.xz">mesa-26.2.2.tar.xz</a></td><td align="right">2026-09-02 15:53  </td></tr>
<tr><td><a href="mesa-26.2.2.tar.xz.sig">mesa-26.2.2.tar.xz.sig</a></td><td align="right">2026-09-02 15:53  </td></tr>
<tr><td><a href="mesa-26.3.0-rc1.tar.xz">mesa-26.3.0-rc1.tar.xz</a></td><td align="right">2026-09-16 10:00  </td></tr>"#;
        let entries = parse_file_index(html, r"mesa-(\d+\.\d+\.\d+(?:-rc\d+)?)\.tar\.xz");
        let versions: Vec<&str> = entries.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(
            versions,
            vec!["26.2.2", "26.3.0-rc1"],
            "signatures are not releases"
        );
        assert_eq!(entries[0].date, d("2026-09-02"));
    }

    /// nginx-style listing, as kernel.org serves linux-firmware.
    #[test]
    fn parses_an_nginx_index() {
        let html = r#"<a href="linux-firmware-20260910.tar.xz">linux-firmware-20260910.tar.xz</a>                     10-Sep-2026 11:23    512M
<a href="linux-firmware-20260916.tar.xz">linux-firmware-20260916.tar.xz</a>                     16-Sep-2026 15:13    512M"#;
        let entries = parse_file_index(html, r"linux-firmware-(\d{8})\.tar\.xz");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].date, d("2026-09-16"));
    }

    fn releases() -> Releases {
        serde_json::from_str(
            r#"{"releases": [
            {"version":"7.3-rc4","moniker":"mainline","iseol":false,"released":{"isodate":"2026-09-20"}},
            {"version":"7.2.7","moniker":"stable","iseol":false,"released":{"isodate":"2026-09-21"}},
            {"version":"7.1.13","moniker":"stable","iseol":true,"released":{"isodate":"2026-09-02"}}
        ]}"#,
        )
        .unwrap()
    }

    fn index() -> Vec<IndexEntry> {
        [
            ("7.0", "2026-04-12"),
            ("7.0.12", "2026-05-30"),
            ("7.1", "2026-06-14"),
            ("7.1.1", "2026-06-19"),
            ("7.1.13", "2026-09-02"),
            ("7.2", "2026-08-17"),
            ("7.2.1", "2026-08-27"),
            ("7.2.6", "2026-09-14"),
            ("7.2.7", "2026-09-21"),
        ]
        .iter()
        .map(|(v, dt)| IndexEntry {
            version: (*v).into(),
            date: d(dt),
        })
        .collect()
    }

    #[test]
    fn kernel_lanes_run_from_development_to_eol() {
        let lanes = kernel_lanes(
            &releases(),
            &index(),
            &re(),
            &BTreeSet::new(),
            d("2026-09-22"),
        );
        let names: Vec<&str> = lanes.iter().map(|l| l.series.as_str()).collect();
        assert_eq!(names, vec!["7.3", "7.2", "7.1", "7.0"], "newest first");

        let dev = &lanes[0];
        assert_eq!(dev.state, LaneState::Development);
        assert_eq!(
            dev.start,
            Some(d("2026-08-17")),
            "the merge window opens with 7.2"
        );
        assert_eq!(dev.releases.len(), 4, "rc1..rc4");
        assert!(dev.releases[0].inferred && !dev.releases[3].inferred);
        assert_eq!(dev.releases[0].date, d("2026-08-30"));
        let fin = dev
            .projections
            .iter()
            .find(|p| p.kind == ProjectionKind::NextMainline)
            .unwrap();
        assert_eq!(fin.date, d("2026-10-18"), "rc4 + four weeks");

        let active = &lanes[1];
        assert_eq!(active.state, LaneState::Active);
        assert_eq!(active.releases[0].kind, ReleaseKind::Mainline);
        let next = &active.projections[0];
        assert_eq!(next.label, "7.2.8");
        assert!(next.date > d("2026-09-21"));

        assert_eq!(lanes[2].state, LaneState::Eol);
        assert_eq!(lanes[2].end, Some(d("2026-09-02")));
        assert!(
            lanes[2].projections.is_empty(),
            "an EOL series has no next release"
        );
    }

    #[test]
    fn index_lanes_split_series_and_project_the_next() {
        let entries: Vec<IndexEntry> = [
            ("26.1.8", "2026-07-30"),
            ("26.2.0-rc1", "2026-07-15"),
            ("26.2.0", "2026-08-06"),
            ("26.2.1", "2026-08-20"),
            ("26.2.2", "2026-09-02"),
            ("26.3.0-rc1", "2026-09-16"),
        ]
        .iter()
        .map(|(v, dt)| IndexEntry {
            version: (*v).into(),
            date: d(dt),
        })
        .collect();
        let lanes = index_lanes(&entries, &re(), &BTreeSet::new(), d("2026-09-22"));
        let names: Vec<&str> = lanes.iter().map(|l| l.series.as_str()).collect();
        assert_eq!(names, vec!["26.3", "26.2", "26.1"]);
        assert_eq!(lanes[0].state, LaneState::Development);
        assert_eq!(lanes[1].state, LaneState::Active);
        assert_eq!(lanes[1].releases[1].kind, ReleaseKind::Mainline);
        assert_eq!(lanes[1].projections[0].label, "26.2.3");
    }

    #[test]
    fn date_versioned_packages_share_one_lane() {
        let entries: Vec<IndexEntry> = [("20260810", "2026-08-10"), ("20260916", "2026-09-16")]
            .iter()
            .map(|(v, dt)| IndexEntry {
                version: (*v).into(),
                date: d(dt),
            })
            .collect();
        let lanes = index_lanes(&entries, &re(), &BTreeSet::new(), d("2026-09-22"));
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].series, "");
        assert_eq!(lanes[0].projections[0].label, "next");
    }

    #[test]
    fn relevance_follows_loaded_drivers() {
        let r = Relevance::from_modules(["amdgpu", "iwlwifi", "drm", "kvm_amd"]);
        assert_eq!(r.driver_of("drm/amdgpu: fix a leak"), Some("amdgpu"));
        assert_eq!(r.driver_of("drm/amd/display: fix DSC"), Some("amdgpu"));
        assert_eq!(r.driver_of("drm/amd/display/dc: fix DSC"), Some("amdgpu"));
        assert_eq!(
            r.driver_of("iommu/amd: fix"),
            None,
            "an AMD IOMMU is not the GPU"
        );
        assert_eq!(r.driver_of("ASoC: amd: acp: fix"), None);
        assert_eq!(r.driver_of("wifi: iwlwifi: mvm: fix scan"), Some("iwlwifi"));
        assert_eq!(r.driver_of("KVM: SVM: fix nested"), Some("kvm_amd"));
        assert_eq!(r.driver_of("radv: fix NGG culling"), Some("amdgpu"));
        assert_eq!(r.driver_of("drm/i915: fix"), None, "not loaded here");
        assert_eq!(
            r.driver_of("drm: fix a core thing"),
            None,
            "core modules are not a signal"
        );
        let sched = Relevance::from_modules(["gpu_sched"]);
        assert_eq!(sched.driver_of("net/sched: fq: clamp"), None);
        assert_eq!(sched.driver_of("drm/sched: fix a race"), Some("gpu_sched"));
    }

    #[test]
    fn hardware_outranks_merely_loaded_modules() {
        // name size refcount used-by state address
        let proc_modules = "mt7925e 16384 0 - Live 0x0\n\
                            mt76 110592 3 mt7925e, Live 0x0\n\
                            nf_tables 380928 1 - Live 0x0\n\
                            snd_hda_codec_realtek 200704 1 - Live 0x0\n";
        let bound: BTreeSet<String> = ["mt7925e".to_string()].into();
        let r = Relevance::from_system(proc_modules, &bound, ["btrfs", "tmpfs"]);
        assert_eq!(
            r.classify("wifi: mt76: mt7925: fix a leak"),
            Some(("mt7925e", Tier::Hardware))
        );
        assert_eq!(
            r.classify("wifi: mt76: fix the library"),
            Some(("mt76", Tier::Hardware)),
            "a dependency of a bound driver"
        );
        assert_eq!(
            r.classify("wifi: mt76: mt7996: fix"),
            None,
            "another chip that shares the library"
        );
        assert_eq!(
            r.classify("btrfs: fix a race"),
            Some(("btrfs", Tier::Hardware)),
            "a mounted filesystem"
        );
        assert_eq!(
            r.classify("netfilter: nf_tables: fix"),
            Some(("nf_tables", Tier::Loaded))
        );
        assert_eq!(
            r.classify("ALSA: hda/realtek: add a quirk"),
            Some(("snd_hda_codec_realtek", Tier::Loaded))
        );

        let (relevant, notable) = r.tally(
            [
                "netfilter: nf_tables: a",
                "netfilter: nf_tables: b",
                "wifi: mt76: mt7925: c",
            ],
            10,
        );
        assert_eq!(
            relevant[0],
            ("mt7925e".to_string(), 1, Tier::Hardware),
            "hardware first even with fewer changes"
        );
        assert_eq!(notable[0], "wifi: mt76: mt7925: c");
    }

    #[test]
    fn a_kernel_changelog_is_shaped_for_this_machine() {
        let text = "commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nAuthor: A <a@example.com>\n\n    drm/amdgpu: fix display wake\n\n    Body mentioning iwlwifi.\n\ncommit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nAuthor: B <b@example.com>\n\n    net: fix an unrelated thing\n";
        let r = Relevance::from_modules(["amdgpu", "iwlwifi"]);
        let s = kernel_shape(text, &BTreeMap::new(), &r);
        assert_eq!(s.patches, 2);
        assert_eq!(
            s.relevant,
            vec![("amdgpu".to_string(), 1, Tier::Hardware)],
            "bodies do not count"
        );
        assert_eq!(s.notable, vec!["drm/amdgpu: fix display wake"]);
    }

    /// A stable backport is reverted in a later point release. The revert
    /// names the stable commit; the original carries its upstream id too.
    #[test]
    fn a_later_revert_is_matched_to_the_change_it_undoes() {
        let r = Relevance::from_modules(["amdgpu"]);
        let v5 = "commit 1111111111111111111111111111111111111111\nAuthor: A <a@example.com>\n\n    drm/amdgpu: enable the shiny thing\n\n    [ Upstream commit 9999999999999999999999999999999999999999 ]\n\ncommit 2222222222222222222222222222222222222222\nAuthor: B <b@example.com>\n\n    drm/amdgpu: fix a leak\n";
        let v6 = "commit 3333333333333333333333333333333333333333\nAuthor: C <c@example.com>\n\n    Revert \"drm/amdgpu: enable the shiny thing\"\n\n    This reverts commit 111111111111.\n";
        let (s5, s6) = (
            kernel_shape(v5, &BTreeMap::new(), &r),
            kernel_shape(v6, &BTreeMap::new(), &r),
        );
        assert_eq!(s5.changes.len(), 2);
        assert_eq!(s5.changes[0].ids.len(), 2, "own id and upstream id");
        assert_eq!(s6.reverts_of.len(), 1);

        let watch = revert_watch(&[("7.2.5", &s5), ("7.2.6", &s6)]);
        let w5 = &watch["7.2.5"];
        assert_eq!(w5.reverted_later.len(), 1);
        assert_eq!(
            w5.reverted_later[0].subject,
            "drm/amdgpu: enable the shiny thing"
        );
        assert_eq!(w5.reverted_later[0].by, "7.2.6");
        assert_eq!(watch["7.2.6"].reverts_earlier[0].2, "7.2.5");
    }

    #[test]
    fn subjects_match_when_there_is_no_id() {
        let change = ChangeRef {
            subject: "radv: fix NGG culling".into(),
            driver: "amdgpu".into(),
            tier: Tier::Hardware,
            ids: vec![],
        };
        let revert = RevertRef {
            subject: "radv: fix NGG culling".into(),
            ids: vec![],
        };
        assert!(revert.undoes(&change));
        let other = RevertRef {
            subject: "radv: something else".into(),
            ids: vec![],
        };
        assert!(!other.undoes(&change));
    }

    #[test]
    fn mesa_release_notes_yield_bugs_and_changes() {
        let html = r#"<section id="bug-fixes"><h2>Bug fixes</h2><ul>
<li><p>Textures glitch in some game</p></li>
<li><p>[radv] GPU hang with NGG culling</p></li></ul></section>
<section id="changes"><h2>Changes</h2><ul>
<li><p>Some Developer:</p><ul>
<li><p>radv: fix NGG culling</p></li>
<li><p>anv: fix something Intel</p></li>
<li><p>Revert &quot;aco: break things&quot;</p></li></ul></li></ul></section>"#;
        let r = Relevance::from_modules(["amdgpu"]);
        let s = mesa_shape(html, &BTreeMap::new(), &r);
        assert_eq!(s.patches, 3);
        assert_eq!(s.reverts, 1);
        assert_eq!(s.relevant, vec![("amdgpu".to_string(), 1, Tier::Hardware)]);
        assert_eq!(
            s.notable[0], "[radv] GPU hang with NGG culling",
            "relevant bugs first"
        );
    }
}
