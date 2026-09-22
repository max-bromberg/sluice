//! Upstream release lineage.
//!
//! The point of this view is to answer "how mature is the series I am being
//! asked to jump to?" before the jump, not after. It shows the shape of each
//! point release — how many patches, how many touching the subsystems you care
//! about, how many reverts — and whether upstream still supports the series you
//! are sitting on. It offers a maturity *hint* and never a decision.
//!
//! Everything is cached, and the whole view renders from cache when offline,
//! stating how stale it is rather than silently showing old data as current.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{ComponentConfig, LineageConfig};
use crate::version::Evr;

// ---------------------------------------------------------------------------
// kernel.org releases.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReleaseStamp {
    #[serde(default)]
    pub isodate: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub moniker: String,
    #[serde(default)]
    pub iseol: bool,
    #[serde(default)]
    pub released: Option<ReleaseStamp>,
}

impl Release {
    pub fn date(&self) -> Option<NaiveDate> {
        self.released
            .as_ref()?
            .isodate
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    }

    pub fn series(&self, re: &Regex) -> Option<String> {
        re.captures(&self.version)?
            .get(1)
            .map(|m| m.as_str().to_string())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Releases {
    #[serde(default)]
    pub releases: Vec<Release>,
}

impl Releases {
    /// Upstream's view of a series as `releases.json` states it, if it lists
    /// the series at all. kernel.org lists one entry per *active* branch, so a
    /// series that has aged out is simply absent; see [`series_status`].
    pub fn listed(&self, series: &str, re: &Regex) -> Option<&Release> {
        self.releases
            .iter()
            .find(|r| r.series(re).as_deref() == Some(series))
    }

    /// The oldest non-longterm series still listed. Anything older that is not
    /// longterm has reached end of life.
    fn oldest_stable(&self, re: &Regex) -> Option<Evr> {
        self.releases
            .iter()
            .filter(|r| r.moniker == "stable" || r.moniker == "mainline")
            .filter_map(|r| r.series(re).map(|s| Evr::parse(&s)))
            .min()
    }
}

/// Upstream's view of one series, assembled from `releases.json` and the
/// release index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesStatus {
    pub series: String,
    /// The newest release, e.g. `7.2.7`, or `7.3-rc4` for an unreleased series.
    pub latest: String,
    pub moniker: String,
    pub eol: bool,
    /// True when EOL was inferred from the series having dropped out of
    /// `releases.json`, rather than stated by an `iseol` flag.
    pub eol_inferred: bool,
    /// When the series stopped receiving releases: the date of its last one.
    pub eol_since: Option<NaiveDate>,
    /// The mainline release date (`7.2`), which starts the series.
    pub released: Option<NaiveDate>,
}

impl SeriesStatus {
    pub fn is_longterm(&self) -> bool {
        self.moniker.contains("longterm")
    }

    pub fn is_prerelease(&self) -> bool {
        self.latest.contains("-rc")
    }

    pub fn describe(&self) -> String {
        if self.eol {
            let how = if self.eol_inferred {
                "EOL upstream (no longer listed"
            } else {
                "EOL upstream (last"
            };
            match (self.eol_inferred, &self.eol_since) {
                (true, Some(d)) => format!("{how}; last release {} on {d})", self.latest),
                (true, None) => format!("{how})"),
                (false, _) => format!("{how}: {})", self.latest),
            }
        } else if self.is_prerelease() {
            format!("mainline {} (not yet released)", self.latest)
        } else if self.is_longterm() {
            format!("longterm, current {}", self.latest)
        } else {
            format!("active, current {}", self.latest)
        }
    }
}

/// Combine `releases.json` and the release index into one status.
pub fn series_status(
    releases: &Releases,
    index: &[IndexEntry],
    series: &str,
    re: &Regex,
) -> Option<SeriesStatus> {
    let points = points_of(index, series);
    let released = index.iter().find(|e| e.version == series).map(|e| e.date);
    let last_point = points.last();

    if let Some(r) = releases.listed(series, re) {
        return Some(SeriesStatus {
            series: series.to_string(),
            latest: r.version.clone(),
            moniker: r.moniker.clone(),
            eol: r.iseol,
            eol_inferred: false,
            eol_since: r
                .iseol
                .then(|| r.date().or(last_point.map(|p| p.date)))
                .flatten(),
            released,
        });
    }

    // Not listed. With index data it once existed; whether it is EOL depends
    // on it being older than every active stable series.
    let latest = last_point
        .map(|p| p.version.clone())
        .or_else(|| released.map(|_| series.to_string()))?;
    let older = releases
        .oldest_stable(re)
        .is_some_and(|oldest| Evr::parse(series) < oldest);
    Some(SeriesStatus {
        series: series.to_string(),
        latest,
        moniker: String::new(),
        eol: older,
        eol_inferred: older,
        eol_since: if older {
            last_point.map(|p| p.date).or(released)
        } else {
            None
        },
        released,
    })
}

// ---------------------------------------------------------------------------
// The release index
// ---------------------------------------------------------------------------

/// One `ChangeLog-<version>` file in the cdn.kernel.org directory index. Its
/// date is the release date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub version: String,
    pub date: NaiveDate,
}

/// Parse the `v7.x/` directory listing. `releases.json` lists only the newest
/// release of each active branch, so this is where the full lineage of point
/// releases and their dates comes from.
pub fn parse_index(html: &str) -> Vec<IndexEntry> {
    let re = Regex::new(r#"href="ChangeLog-(\d+\.\d+(?:\.\d+)?)"[^\n]*?(\d{2}-[A-Za-z]{3}-\d{4})"#)
        .expect("static regex");
    let mut out: Vec<IndexEntry> = re
        .captures_iter(html)
        .filter_map(|c| {
            Some(IndexEntry {
                version: c[1].to_string(),
                date: NaiveDate::parse_from_str(&c[2], "%d-%b-%Y").ok()?,
            })
        })
        .collect();
    out.sort_by(|a, b| Evr::parse(&a.version).cmp(&Evr::parse(&b.version)));
    out
}

/// The point releases of `series` (`7.2.1`, `7.2.2`, …), oldest first. The
/// mainline release itself is not one: its ChangeLog is the whole merge window.
pub fn points_of<'a>(index: &'a [IndexEntry], series: &str) -> Vec<&'a IndexEntry> {
    let prefix = format!("{series}.");
    index
        .iter()
        .filter(|e| {
            e.version
                .strip_prefix(&prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Changelog shape
// ---------------------------------------------------------------------------

/// The measurable shape of one point release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointRelease {
    pub version: String,
    pub date: Option<NaiveDate>,
    pub patches: usize,
    pub reverts: usize,
    /// Counts for each configured `highlight` regex, by label.
    pub highlights: BTreeMap<String, usize>,
}

/// The newest `n` entries of an RPM changelog, each cut to a few lines.
pub fn newest_rpm_entries(text: &str, n: usize) -> Vec<String> {
    let mut entries: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line.starts_with("* ") {
            if entries.len() == n {
                break;
            }
            entries.push(vec![line]);
        } else if let Some(e) = entries.last_mut() {
            if !line.trim().is_empty() && e.len() < 6 {
                e.push(line);
            }
        }
    }
    entries.into_iter().map(|e| e.join("\n")).collect()
}

/// Count commits, reverts and highlight hits in a kernel.org ChangeLog, which
/// is `git log` output.
pub fn analyse_changelog(text: &str, highlights: &BTreeMap<String, Regex>) -> ChangelogShape {
    let commit = Regex::new(r"(?m)^commit [0-9a-f]{7,40}$").expect("static regex");
    let patches = commit.find_iter(text).count();

    // Subject lines are the indented lines of a git log entry. Counting matches
    // over subjects rather than the whole body keeps a passing mention in a
    // commit message from inflating the count.
    let subjects: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("    ") && !l.trim().is_empty())
        .collect();

    let reverts = subjects
        .iter()
        .filter(|l| l.trim_start().starts_with("Revert "))
        .count();

    let mut counts = BTreeMap::new();
    for (label, re) in highlights {
        counts.insert(
            label.clone(),
            subjects.iter().filter(|l| re.is_match(l)).count(),
        );
    }

    ChangelogShape {
        patches,
        reverts,
        highlights: counts,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangelogShape {
    pub patches: usize,
    pub reverts: usize,
    pub highlights: BTreeMap<String, usize>,
}

// ---------------------------------------------------------------------------
// The assembled view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SeriesLineage {
    pub series: String,
    pub status: Option<SeriesStatus>,
    pub points: Vec<PointRelease>,
    /// Earlier point releases left out to bound the number of fetches.
    pub omitted: usize,
    /// The advisory maturity hint, for the candidate series.
    pub hint: Option<String>,
    /// What the distribution's side looks like, e.g. how long it has offered
    /// the candidate.
    pub repo_note: Option<String>,
    /// The newest entries of the distribution package's own changelog, when
    /// `lineage.repo_changelog` is on.
    pub repo_changelog: Vec<String>,
    /// Set when the repositories no longer ship this series.
    pub withdrawn: bool,
    pub is_current: bool,
    /// The series is actually held at the gate, rather than merely asked about.
    pub gated: bool,
}

impl SeriesLineage {
    /// The highest point-release number seen for this series.
    pub fn max_point(&self, re: &Regex) -> Option<u32> {
        self.points
            .iter()
            .filter_map(|p| {
                let series = re.captures(&p.version)?.get(1)?.as_str();
                p.version
                    .strip_prefix(series)?
                    .trim_start_matches('.')
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()
            })
            .max()
    }

    /// A maturity hint. Deliberately advisory: sluice never promotes on it.
    pub fn hint(&self, cfg: &ComponentConfig, re: &Regex) -> Option<String> {
        let threshold = cfg.promote_hint_min_point?;
        let max = self.max_point(re)?;
        if max < threshold {
            return Some(format!(
                "below your maturity threshold (.{max} < .{threshold})"
            ));
        }
        let recent_reverts: usize = self.points.iter().rev().take(2).map(|p| p.reverts).sum();
        Some(match recent_reverts {
            0 => {
                format!("maturity threshold reached (.{max}), no reverts in the last two releases")
            }
            n => format!(
                "maturity threshold reached (.{max}), but {n} revert(s) in the last two releases"
            ),
        })
    }
}

#[derive(Debug, Clone)]
pub struct LineageView {
    pub candidate: Option<SeriesLineage>,
    pub current: Option<SeriesLineage>,
    /// When the underlying data was fetched, and whether it came from cache.
    pub fetched_at: Option<DateTime<Utc>>,
    pub from_cache: bool,
    /// Non-fatal problems, e.g. a changelog that could not be fetched. The view
    /// is still rendered; upstream data is never allowed to block a decision.
    pub warnings: Vec<String>,
}

impl LineageView {
    pub fn staleness(&self) -> Option<chrono::Duration> {
        self.fetched_at.map(|t| Utc::now() - t)
    }

    /// How old the data is, phrased for a human.
    pub fn freshness_note(&self) -> String {
        match self.staleness() {
            None => "no upstream data available".into(),
            Some(_) if !self.from_cache => {
                format!("fetched just now ({} warnings)", self.warnings.len())
            }
            Some(age) if age.num_hours() < 1 => {
                format!("cached {} min ago", age.num_minutes().max(0))
            }
            Some(age) if age.num_days() < 1 => format!("cached {} h ago", age.num_hours()),
            Some(age) => format!("cached {} days ago", age.num_days()),
        }
    }
}

// ---------------------------------------------------------------------------
// Fetching and caching
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct CacheEnvelope {
    fetched_at: DateTime<Utc>,
    body: String,
}

pub struct Fetcher<'a> {
    cfg: &'a LineageConfig,
    cache_dir: PathBuf,
    /// The system cache, still read when an unprivileged run has to write its
    /// own cache elsewhere.
    fallback_dir: Option<PathBuf>,
    pub warnings: Vec<String>,
    pub used_cache: bool,
    pub oldest_fetch: Option<DateTime<Utc>>,
}

impl<'a> Fetcher<'a> {
    pub fn new(cfg: &'a LineageConfig, cache_dir: impl Into<PathBuf>) -> Self {
        let (cache_dir, fallback_dir) = resolve_cache_dir(cache_dir.into());
        Fetcher {
            cfg,
            cache_dir,
            fallback_dir,
            warnings: Vec::new(),
            used_cache: false,
            oldest_fetch: None,
        }
    }

    /// Fetch `url`, falling back to cache on any failure.
    ///
    /// Cache is preferred while it is inside the TTL. Beyond that a fetch is
    /// attempted, but a stale cache entry still beats no data: the view is
    /// meant to work on a machine whose network is down.
    pub fn get(&mut self, url: &str) -> Option<String> {
        self.fetch(url, false)
    }

    /// Like [`get`](Self::get), for a document that never changes once
    /// published — a released ChangeLog. Any cached copy is used, however old.
    pub fn get_immutable(&mut self, url: &str) -> Option<String> {
        self.fetch(url, true)
    }

    /// Where downloaded artifacts (RPMs for the changelog view) go.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn read_cached(&self, url: &str) -> Option<CacheEnvelope> {
        let name = format!("{}.json", slug(url));
        let primary = read_cache(&self.cache_dir.join(&name));
        let fallback = self
            .fallback_dir
            .as_ref()
            .and_then(|d| read_cache(&d.join(&name)));
        match (primary, fallback) {
            (Some(a), Some(b)) => Some(if a.fetched_at >= b.fetched_at { a } else { b }),
            (a, b) => a.or(b),
        }
    }

    fn fetch(&mut self, url: &str, immutable: bool) -> Option<String> {
        let path = self.cache_path(url);

        if let Some(env) = self.read_cached(url) {
            let age = Utc::now() - env.fetched_at;
            if immutable
                || self.cfg.offline
                || age.num_hours() < i64::from(self.cfg.cache_ttl_hours)
            {
                self.note_cache_hit(env.fetched_at);
                return Some(env.body);
            }
        }

        if self.cfg.offline {
            self.warnings
                .push(format!("offline, and {url} is not cached"));
            return None;
        }

        match http_get(url, self.cfg.http_timeout_secs) {
            Ok(body) => {
                let now = Utc::now();
                if let Err(e) = write_cache(
                    &path,
                    &CacheEnvelope {
                        fetched_at: now,
                        body: body.clone(),
                    },
                ) {
                    self.warnings.push(format!("could not cache {url}: {e}"));
                }
                self.oldest_fetch = Some(match self.oldest_fetch {
                    Some(t) => t.min(now),
                    None => now,
                });
                Some(body)
            }
            Err(e) => {
                self.warnings.push(format!("fetching {url}: {e}"));
                // Any cache at all, however stale, is better than nothing here.
                self.read_cached(url).map(|env| {
                    self.note_cache_hit(env.fetched_at);
                    env.body
                })
            }
        }
    }

    fn note_cache_hit(&mut self, at: DateTime<Utc>) {
        self.used_cache = true;
        self.oldest_fetch = Some(match self.oldest_fetch {
            Some(t) => t.min(at),
            None => at,
        });
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.json", slug(url)))
    }

    pub fn releases(&mut self) -> Option<Releases> {
        let body = self.get(&self.cfg.releases_url.clone())?;
        match serde_json::from_str::<Releases>(&body) {
            Ok(r) => Some(r),
            Err(e) => {
                self.warnings.push(format!("parsing releases.json: {e}"));
                None
            }
        }
    }

    /// The directory index for a major version, e.g. `v7.x/`.
    pub fn index(&mut self, major: &str) -> Option<Vec<IndexEntry>> {
        let url = format!("{}/v{major}.x/", self.cfg.changelog_base);
        let html = self.get(&url)?;
        let entries = parse_index(&html);
        if entries.is_empty() {
            self.warnings.push(format!(
                "{url} listed no ChangeLogs; its format may have changed"
            ));
        }
        Some(entries)
    }

    /// `https://cdn.kernel.org/pub/linux/kernel/v7.x/ChangeLog-7.3.2`
    pub fn changelog(&mut self, version: &str) -> Option<String> {
        let major = version.split('.').next()?;
        let url = format!("{}/v{major}.x/ChangeLog-{version}", self.cfg.changelog_base);
        self.get_immutable(&url)
    }
}

/// Use the configured cache if this process can write to it; otherwise (an
/// unprivileged run against a root-owned cache) write to the user's cache and
/// keep reading the configured one.
pub fn resolve_cache_dir(configured: PathBuf) -> (PathBuf, Option<PathBuf>) {
    let writable = std::fs::create_dir_all(&configured).is_ok() && {
        let probe = configured.join(".sluice-write-probe");
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    };
    if writable {
        return (configured, None);
    }
    match dirs::cache_dir() {
        Some(user) => (user.join("sluice"), Some(configured)),
        None => (configured, None),
    }
}

/// A filesystem-safe, collision-resistant name for a URL.
fn slug(url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let tail: String = url
        .rsplit('/')
        .next()
        .unwrap_or("url")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .take(48)
        .collect();
    format!("{tail}-{hash:016x}")
}

fn read_cache(path: &Path) -> Option<CacheEnvelope> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(path: &Path, env: &CacheEnvelope) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(env)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn http_get(url: &str, timeout_secs: u64) -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(concat!("sluice/", env!("CARGO_PKG_VERSION")))
        .build();
    let body = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re() -> Regex {
        Regex::new(r"^(\d+\.\d+)").unwrap()
    }

    #[allow(dead_code)]
    const RELEASES: &str = r#"{
      "releases": [
        {"version":"7.3.3","moniker":"stable","iseol":false,"released":{"isodate":"2026-10-20"}},
        {"version":"7.3.2","moniker":"stable","iseol":false,"released":{"isodate":"2026-10-13"}},
        {"version":"7.2.6","moniker":"stable","iseol":true,"released":{"isodate":"2026-09-15"}},
        {"version":"7.0.12","moniker":"longterm","iseol":false,"released":{"isodate":"2026-06-01"}}
      ]
    }"#;

    /// The shape kernel.org actually serves: one entry per active branch.
    const RELEASES_REAL: &str = r#"{"releases": [
        {"version":"7.3-rc4","moniker":"mainline","iseol":false,"released":{"isodate":"2026-09-20"}},
        {"version":"7.2.7","moniker":"stable","iseol":false,"released":{"isodate":"2026-09-21"}},
        {"version":"7.1.13","moniker":"stable","iseol":true,"released":{"isodate":"2026-09-02"}},
        {"version":"6.18.53","moniker":"longterm","iseol":false,"released":{"isodate":"2026-09-21"}}
    ]}"#;

    /// Lines verbatim from cdn.kernel.org/pub/linux/kernel/v7.x/.
    const INDEX: &str = r#"<a href="ChangeLog-7.0">ChangeLog-7.0</a>                                      12-Apr-2026 18:02     18M
<a href="ChangeLog-7.0.12">ChangeLog-7.0.12</a>                                   30-May-2026 09:10    120K
<a href="ChangeLog-7.1">ChangeLog-7.1</a>                                      14-Jun-2026 17:40     17M
<a href="ChangeLog-7.1.1">ChangeLog-7.1.1</a>                                    19-Jun-2026 12:13     14K
<a href="ChangeLog-7.1.2">ChangeLog-7.1.2</a>                                    27-Jun-2026 10:30     24K
<a href="ChangeLog-7.1.13">ChangeLog-7.1.13</a>                                   02-Sep-2026 11:00     40K
<a href="ChangeLog-7.2">ChangeLog-7.2</a>                                      17-Aug-2026 04:30     19M
<a href="ChangeLog-7.2.1">ChangeLog-7.2.1</a>                                    22-Aug-2026 10:00     30K
<a href="ChangeLog-7.2.10">ChangeLog-7.2.10</a>                                   30-Sep-2026 10:00     30K
<a href="ChangeLog-7.2.2">ChangeLog-7.2.2</a>                                    29-Aug-2026 10:00     30K
<a href="linux-7.2.tar.xz">linux-7.2.tar.xz</a>                                   17-Aug-2026 04:30    153M
"#;

    #[test]
    fn the_index_lists_point_releases_in_version_order() {
        let index = parse_index(INDEX);
        let points: Vec<&str> = points_of(&index, "7.2")
            .iter()
            .map(|e| e.version.as_str())
            .collect();
        assert_eq!(
            points,
            vec!["7.2.1", "7.2.2", "7.2.10"],
            "numeric, not lexical, and no mainline"
        );
        assert_eq!(
            index.iter().find(|e| e.version == "7.2").unwrap().date,
            NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()
        );
    }

    #[test]
    fn series_status_combines_both_sources() {
        let r: Releases = serde_json::from_str(RELEASES_REAL).unwrap();
        let index = parse_index(INDEX);

        let active = series_status(&r, &index, "7.2", &re()).unwrap();
        assert_eq!(active.latest, "7.2.7");
        assert!(!active.eol);
        assert_eq!(active.released, NaiveDate::from_ymd_opt(2026, 8, 17));

        let stated = series_status(&r, &index, "7.1", &re()).unwrap();
        assert!(stated.eol && !stated.eol_inferred);
        assert_eq!(stated.eol_since, NaiveDate::from_ymd_opt(2026, 9, 2));

        // 7.0 has dropped out of releases.json entirely. Silence is not "fine":
        // it is older than every active stable series, so it is EOL.
        let gone = series_status(&r, &index, "7.0", &re()).unwrap();
        assert!(gone.eol && gone.eol_inferred, "{gone:?}");
        assert_eq!(gone.latest, "7.0.12");
        assert_eq!(gone.eol_since, NaiveDate::from_ymd_opt(2026, 5, 30));

        let lts = series_status(&r, &index, "6.18", &re()).unwrap();
        assert!(lts.is_longterm() && !lts.eol);

        let next = series_status(&r, &index, "7.3", &re()).unwrap();
        assert!(next.is_prerelease());
        assert!(next.describe().contains("not yet released"));

        assert!(series_status(&r, &index, "9.9", &re()).is_none());
    }

    #[test]
    fn releases_json_tolerates_missing_fields() {
        let r: Releases = serde_json::from_str(r#"{"releases":[{"version":"7.3.3"}]}"#).unwrap();
        assert_eq!(r.releases[0].date(), None);
        assert!(!r.releases[0].iseol);
    }

    const CHANGELOG: &str = "\
commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Author: A Dev <a@example.com>
Date:   Mon Oct 20 10:00:00 2026 +0200

    drm/amdgpu: fix display wake after DPMS off

commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
Author: B Dev <b@example.com>
Date:   Mon Oct 20 11:00:00 2026 +0200

    Revert \"drm/amd/display: enable DPMS shortcut\"

    This reverts commit cccccccccccccccccccccccccccccccccccccccc.

commit dddddddddddddddddddddddddddddddddddddddd
Author: C Dev <c@example.com>
Date:   Mon Oct 20 12:00:00 2026 +0200

    net: fix an unrelated thing
";

    #[test]
    fn measures_changelog_shape() {
        let mut highlights = BTreeMap::new();
        highlights.insert(
            "amdgpu".to_string(),
            Regex::new(r"drm/amd|amdgpu|amdkfd").unwrap(),
        );

        let shape = analyse_changelog(CHANGELOG, &highlights);
        assert_eq!(shape.patches, 3);
        assert_eq!(shape.reverts, 1);
        // Two subjects mention amdgpu; the revert's body line must not add a third.
        assert_eq!(shape.highlights["amdgpu"], 2);
    }

    #[test]
    fn takes_the_newest_rpm_changelog_entries() {
        let text = "* Mon Sep 21 2026 dev@example.com\n- Linux 7.3.3\n- refresh patches\n\n\
                    * Mon Sep 14 2026 dev@example.com\n- Linux 7.3.2\n\n\
                    * Mon Sep 07 2026 dev@example.com\n- Linux 7.3.1\n";
        let entries = newest_rpm_entries(text, 2);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("7.3.3") && entries[0].contains("refresh patches"));
        assert!(entries[1].contains("7.3.2"));
    }

    #[test]
    fn empty_changelog_measures_as_zero_rather_than_failing() {
        let shape = analyse_changelog("", &BTreeMap::new());
        assert_eq!(shape, ChangelogShape::default());
    }

    fn lineage(points: &[(&str, usize)]) -> SeriesLineage {
        SeriesLineage {
            series: "7.3".into(),
            status: None,
            omitted: 0,
            hint: None,
            repo_note: None,
            repo_changelog: Vec::new(),
            withdrawn: false,
            is_current: false,
            gated: true,
            points: points
                .iter()
                .map(|(v, reverts)| PointRelease {
                    version: (*v).into(),
                    reverts: *reverts,
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn maturity_hint_respects_the_configured_threshold() {
        let cfg = ComponentConfig {
            promote_hint_min_point: Some(3),
            ..Default::default()
        };

        let young = lineage(&[("7.3.1", 6), ("7.3.2", 3)]);
        assert_eq!(young.max_point(&re()), Some(2));
        assert!(young
            .hint(&cfg, &re())
            .unwrap()
            .contains("below your maturity threshold"));

        let mature = lineage(&[("7.3.1", 6), ("7.3.2", 0), ("7.3.3", 0)]);
        assert!(mature.hint(&cfg, &re()).unwrap().contains("no reverts"));

        let churny = lineage(&[("7.3.2", 0), ("7.3.3", 4)]);
        assert!(churny.hint(&cfg, &re()).unwrap().contains("4 revert"));
    }

    #[test]
    fn no_hint_without_a_configured_threshold() {
        let cfg = ComponentConfig::default();
        assert!(lineage(&[("7.3.3", 0)]).hint(&cfg, &re()).is_none());
    }

    #[test]
    fn cache_slugs_are_safe_and_distinct() {
        let a = slug("https://cdn.kernel.org/pub/linux/kernel/v7.x/ChangeLog-7.3.2");
        let b = slug("https://cdn.kernel.org/pub/linux/kernel/v7.x/ChangeLog-7.3.3");
        assert_ne!(a, b);
        assert!(a.starts_with("ChangeLog-7.3.2-"));
        assert!(!a.contains('/'));
    }

    #[test]
    fn offline_fetcher_serves_cache_and_never_errors_out() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LineageConfig {
            offline: true,
            ..Default::default()
        };

        let url = "https://example.invalid/releases.json";
        let path = dir.path().join(format!("{}.json", slug(url)));
        write_cache(
            &path,
            &CacheEnvelope {
                fetched_at: Utc::now() - chrono::Duration::days(30),
                body: RELEASES.into(),
            },
        )
        .unwrap();

        let mut f = Fetcher::new(&cfg, dir.path());
        let body = f
            .get(url)
            .expect("stale cache must still be served offline");
        assert!(body.contains("7.3.3"));
        assert!(f.used_cache);

        // A URL with no cache at all is a warning, not a failure.
        assert!(f.get("https://example.invalid/missing").is_none());
        assert_eq!(f.warnings.len(), 1);
    }
}
