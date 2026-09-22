//! Boot entries, ESP capacity, and kernel retention.
//!
//! sluice never edits bootloader configuration directly. It reads state through
//! `bootctl`, changes the default entry through `bootctl set-default`, and
//! leaves entry creation to whatever the distribution already uses
//! (`sdbootutil` or `kernel-install`).
//!
//! Retention matters as much as the bootloader does: `purge-kernels` will
//! happily remove the known-good kernel at boot unless its exact version is
//! listed in `multiversion.kernels`. Pinning it there is what makes rollback a
//! promise rather than a hope.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{BootConfig, BootManager};
use crate::exec::{Cmd, Runner};
use crate::version::Evr;

// ---------------------------------------------------------------------------
// ESP capacity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EspStatus {
    pub path: PathBuf,
    pub total_mb: u64,
    pub free_mb: u64,
}

impl EspStatus {
    pub fn used_mb(&self) -> u64 {
        self.total_mb.saturating_sub(self.free_mb)
    }

    pub fn used_percent(&self) -> u32 {
        if self.total_mb == 0 {
            return 0;
        }
        ((self.used_mb() as f64 / self.total_mb as f64) * 100.0).round() as u32
    }

    pub fn is_low(&self, threshold_mb: u64) -> bool {
        self.free_mb < threshold_mb
    }
}

pub fn esp_status(cfg: &BootConfig, r: &mut Runner) -> Result<Option<EspStatus>> {
    if !cfg.esp_path.exists() {
        return Ok(None);
    }
    // `df -P -k` is POSIX-specified output, so the columns are stable.
    let out = r.run(&Cmd::read("df").arg("-P").arg("-k").arg(&cfg.esp_path))?;
    if !out.ok() {
        return Ok(None);
    }
    Ok(parse_df(&out.stdout).map(|(total_kb, free_kb)| EspStatus {
        path: cfg.esp_path.clone(),
        total_mb: total_kb / 1024,
        free_mb: free_kb / 1024,
    }))
}

/// Pull (1K-blocks, available) from `df -P -k` output.
fn parse_df(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    if fields.len() < 4 {
        return None;
    }
    Some((fields[1].parse().ok()?, fields[3].parse().ok()?))
}

// ---------------------------------------------------------------------------
// Boot entries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootEntry {
    pub id: String,
    pub title: Option<String>,
    pub version: Option<String>,
    pub is_default: bool,
    pub is_selected: bool,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "showTitle")]
    show_title: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "isDefault")]
    is_default: bool,
    #[serde(default, rename = "isSelected")]
    is_selected: bool,
}

/// List boot entries via `bootctl list --json=short`.
///
/// Returns `Ok(None)` when the entries cannot be read — typically because the
/// ESP is only readable by root. That is a normal state for an unprivileged
/// `status` run, not an error worth failing on.
pub fn entries(cfg: &BootConfig, r: &mut Runner) -> Result<Option<Vec<BootEntry>>> {
    if cfg.manager == BootManager::None {
        return Ok(None);
    }
    let out = r.run(&Cmd::read("bootctl").arg("list").arg("--json=short"))?;
    if !out.ok() || out.stdout.trim().is_empty() {
        return Ok(None);
    }
    let raw: Vec<RawEntry> =
        serde_json::from_str(out.stdout.trim()).context("parsing bootctl list JSON")?;
    Ok(Some(
        raw.into_iter()
            .map(|e| BootEntry {
                id: e.id,
                title: e.show_title.or(e.title),
                version: e.version,
                is_default: e.is_default,
                is_selected: e.is_selected,
            })
            .collect(),
    ))
}

/// Whether a kernel release string (`uname -r`, or a boot entry's version,
/// e.g. `7.2.6-1-default`) belongs to the package version `7.2.6-1.1`.
///
/// The package release `1.1` becomes `1` in the kernel release, followed by
/// the flavour, so the comparison is on `<version>-<release major>` with a
/// boundary after it: `7.2.6-1` must not match `7.2.6-10-default`.
pub fn kernel_release_matches(release: &str, evr: &Evr) -> bool {
    let rel_major = evr.release.split('.').next().unwrap_or("");
    let stem = if rel_major.is_empty() {
        evr.version.clone()
    } else {
        format!("{}-{rel_major}", evr.version)
    };
    release
        .strip_prefix(&stem)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// Whether a boot entry id (`opensuse-tumbleweed-7.2.0-1-default-1.conf`)
/// boots the package version `evr`. Used where only the id is known, as with
/// the `LoaderEntryDefault` EFI variable.
pub fn entry_id_boots(id: &str, evr: &Evr) -> bool {
    let rel_major = evr.release.split('.').next().unwrap_or("");
    let needle = format!("-{}-{rel_major}-", evr.version);
    id.match_indices(&needle).next().is_some()
}

/// The entry that boots `version`, chosen from the same snapshot as the
/// current default entry.
///
/// sdbootutil names entries `<token>-<kernel release>-<snapshot>.conf`, one
/// per kernel per snapshot. Picking another snapshot's entry would boot an old
/// root filesystem, so the target id is derived from the current default's id
/// by swapping only the kernel release in it.
pub fn pick_entry<'a>(entries: &'a [BootEntry], version: &Evr) -> Option<&'a BootEntry> {
    let matching: Vec<&BootEntry> = entries
        .iter()
        .filter(|e| {
            e.version.as_deref().map_or_else(
                || entry_id_boots(&e.id, version),
                |v| kernel_release_matches(v, version),
            )
        })
        .collect();

    let default = entries.iter().find(|e| e.is_default);
    if let Some(d) = default {
        if let Some(dv) = d.version.as_deref() {
            let strip = |id: &str| {
                id.split('+')
                    .next()
                    .unwrap_or(id)
                    .trim_end_matches(".conf")
                    .to_string()
            };
            for e in &matching {
                if let Some(ev) = e.version.as_deref() {
                    if strip(&e.id) == strip(&d.id).replacen(dv, ev, 1) {
                        return Some(e);
                    }
                }
            }
        }
    }
    // Without a default to anchor on, only an unambiguous match is safe.
    (matching.len() == 1).then(|| matching[0])
}

/// The default entry id, read from the `LoaderEntryDefault` EFI variable.
///
/// Unlike `bootctl list`, this needs no privileges, so an unprivileged
/// `status` or `check` can still notice that the default has drifted.
pub fn default_entry_from_efivars(efivars_dir: &Path) -> Option<String> {
    const LOADER_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    let raw = std::fs::read(efivars_dir.join(format!("LoaderEntryDefault-{LOADER_GUID}"))).ok()?;
    // Four bytes of attributes, then UTF-16LE with a trailing NUL.
    let units: Vec<u16> = raw
        .get(4..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    let id = String::from_utf16(&units).ok()?;
    (!id.is_empty()).then_some(id)
}

pub fn set_default(r: &mut Runner, entry_id: &str) -> Result<()> {
    r.run(&Cmd::mutate("bootctl").arg("set-default").arg(entry_id))?
        .require_ok("bootctl set-default")?;
    Ok(())
}

pub fn detect_manager(cfg: &BootConfig) -> BootManager {
    match cfg.manager {
        BootManager::Auto => {
            if Path::new("/usr/bin/sdbootutil").exists()
                || Path::new("/usr/sbin/sdbootutil").exists()
            {
                BootManager::Sdbootutil
            } else if Path::new("/usr/bin/kernel-install").exists() {
                BootManager::KernelInstall
            } else {
                BootManager::None
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Kernel retention (multiversion.kernels)
// ---------------------------------------------------------------------------

/// The `multiversion.kernels` setting from a zypp.conf.
///
/// Explicit versions are accepted alongside the symbolic `latest`/`running`
/// keywords, which is what lets a known-good kernel survive `purge-kernels`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Multiversion {
    pub entries: Vec<String>,
}

impl Multiversion {
    pub fn parse(value: &str) -> Self {
        Multiversion {
            entries: value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn render(&self) -> String {
        self.entries.join(",")
    }

    pub fn contains(&self, entry: &str) -> bool {
        self.entries.iter().any(|e| e == entry)
    }

    /// Ensure `base` keywords and every pinned version are present. Returns
    /// true when anything changed.
    pub fn ensure(&mut self, base: &[String], pinned: &[Evr]) -> bool {
        let before = self.entries.clone();
        for b in base {
            if !self.contains(b) {
                self.entries.push(b.clone());
            }
        }
        for p in pinned {
            let s = p.to_string();
            if !self.contains(&s) {
                self.entries.push(s);
            }
        }
        self.entries != before
    }

    /// Drop a pinned version. Symbolic keywords are never removed.
    pub fn unpin(&mut self, version: &Evr) -> bool {
        let s = version.to_string();
        let before = self.entries.len();
        self.entries.retain(|e| e != &s);
        self.entries.len() != before
    }
}

/// Read `multiversion.kernels` from a zypp.conf, ignoring commented-out lines.
pub fn read_multiversion(zypp_conf: &Path) -> Result<Option<Multiversion>> {
    let text = match std::fs::read_to_string(zypp_conf) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", zypp_conf.display())),
    };
    Ok(find_multiversion(&text).map(|(_, v)| Multiversion::parse(&v)))
}

fn find_multiversion(text: &str) -> Option<(usize, String)> {
    text.lines().enumerate().find_map(|(i, line)| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return None;
        }
        let (key, value) = trimmed.split_once('=')?;
        (key.trim() == "multiversion.kernels").then(|| (i, value.trim().to_string()))
    })
}

/// Rewrite `multiversion.kernels` in place, preserving the rest of the file.
pub fn write_multiversion(zypp_conf: &Path, mv: &Multiversion, dry_run: bool) -> Result<bool> {
    let text = std::fs::read_to_string(zypp_conf)
        .with_context(|| format!("reading {}", zypp_conf.display()))?;
    let Some((idx, current)) = find_multiversion(&text) else {
        anyhow::bail!(
            "{} has no active `multiversion.kernels` setting; refusing to invent one",
            zypp_conf.display()
        );
    };
    if Multiversion::parse(&current) == *mv {
        return Ok(false);
    }

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let indent: String = lines[idx]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    lines[idx] = format!("{indent}multiversion.kernels = {}", mv.render());

    if dry_run {
        return Ok(true);
    }

    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    let tmp = zypp_conf.with_extension("conf.sluice-tmp");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, zypp_conf)
        .with_context(|| format!("replacing {}", zypp_conf.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, version: &str, is_default: bool) -> BootEntry {
        BootEntry {
            id: id.into(),
            title: None,
            version: Some(version.into()),
            is_default,
            is_selected: false,
        }
    }

    #[test]
    fn kernel_release_matching_respects_the_release_boundary() {
        let v = Evr::parse("7.2.6-1.1");
        assert!(kernel_release_matches("7.2.6-1-default", &v));
        assert!(!kernel_release_matches("7.2.6-10-default", &v));
        assert!(!kernel_release_matches("7.2.60-1-default", &v));
        assert!(entry_id_boots(
            "opensuse-tumbleweed-7.2.6-1-default-4.conf",
            &v
        ));
        assert!(!entry_id_boots(
            "opensuse-tumbleweed-7.2.6-10-default-4.conf",
            &v
        ));
    }

    /// Entries exist per kernel *per snapshot*. The chosen entry must be in the
    /// snapshot the machine currently boots, or rollback would also roll back
    /// the root filesystem.
    #[test]
    fn picks_the_entry_in_the_current_snapshot() {
        let entries = vec![
            entry(
                "opensuse-tumbleweed-7.2.6-1-default-41.conf",
                "7.2.6-1-default",
                false,
            ),
            entry(
                "opensuse-tumbleweed-7.2.6-1-default-42.conf",
                "7.2.6-1-default",
                false,
            ),
            entry(
                "opensuse-tumbleweed-7.3.3-1-default-41.conf",
                "7.3.3-1-default",
                false,
            ),
            entry(
                "opensuse-tumbleweed-7.3.3-1-default-42+3.conf",
                "7.3.3-1-default",
                true,
            ),
        ];
        let picked = pick_entry(&entries, &Evr::parse("7.2.6-1.1")).unwrap();
        assert_eq!(picked.id, "opensuse-tumbleweed-7.2.6-1-default-42.conf");
    }

    #[test]
    fn an_ambiguous_entry_is_not_guessed() {
        let entries = vec![
            entry("a-7.2.6-1-default-41.conf", "7.2.6-1-default", false),
            entry("a-7.2.6-1-default-42.conf", "7.2.6-1-default", false),
        ];
        assert!(pick_entry(&entries, &Evr::parse("7.2.6-1.1")).is_none());
    }

    #[test]
    fn reads_the_default_entry_efi_variable() {
        let dir = tempfile::tempdir().unwrap();
        let id = "opensuse-tumbleweed-7.2.0-1-default-1.conf";
        let mut raw = vec![7, 0, 0, 0];
        for u in id.encode_utf16().chain(std::iter::once(0)) {
            raw.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(
            dir.path()
                .join("LoaderEntryDefault-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"),
            raw,
        )
        .unwrap();
        assert_eq!(default_entry_from_efivars(dir.path()).as_deref(), Some(id));
        assert!(default_entry_from_efivars(&dir.path().join("nope")).is_none());
    }

    #[test]
    fn parses_df_output() {
        // Verbatim `df -P -k /boot/efi`.
        let text = "Filesystem     1024-blocks   Used Available Capacity Mounted on\n\
                    /dev/sda1          1046512 734208    312304      70% /boot/efi\n";
        let (total, free) = parse_df(text).unwrap();
        assert_eq!(total, 1046512);
        assert_eq!(free, 312304);
    }

    #[test]
    fn esp_status_reports_pressure() {
        let esp = EspStatus {
            path: "/boot/efi".into(),
            total_mb: 1022,
            free_mb: 183,
        };
        assert_eq!(esp.used_mb(), 839);
        assert_eq!(esp.used_percent(), 82);
        assert!(esp.is_low(256), "183 MB free is low against a 256 MB floor");
        assert!(!esp.is_low(128));
    }

    #[test]
    fn parses_bootctl_entries() {
        let json = r#"[
          {"id":"opensuse-7.2.0-1-default.conf","showTitle":"openSUSE Tumbleweed","version":"7.2.0-1-default","isDefault":true,"isSelected":true},
          {"id":"opensuse-7.0.12-1-default.conf","title":"openSUSE (7.0.12)","version":"7.0.12-1-default","isDefault":false,"isSelected":false}
        ]"#;
        let raw: Vec<RawEntry> = serde_json::from_str(json).unwrap();
        let entries: Vec<BootEntry> = raw
            .into_iter()
            .map(|e| BootEntry {
                id: e.id,
                title: e.show_title.or(e.title),
                version: e.version,
                is_default: e.is_default,
                is_selected: e.is_selected,
            })
            .collect();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title.as_deref(), Some("openSUSE Tumbleweed"));
        assert!(entries[0].is_default);

        let found = pick_entry(&entries, &Evr::parse("7.0.12-1.1")).unwrap();
        assert_eq!(found.id, "opensuse-7.0.12-1-default.conf");
        assert!(pick_entry(&entries, &Evr::parse("7.9.9-1.1")).is_none());
    }

    #[test]
    fn multiversion_round_trips_the_real_setting() {
        // A pinned version alongside the symbolic keywords.
        let mv = Multiversion::parse("latest,latest-1,running,7.0.12-1.1");
        assert_eq!(mv.entries.len(), 4);
        assert!(mv.contains("7.0.12-1.1"));
        assert_eq!(mv.render(), "latest,latest-1,running,7.0.12-1.1");
    }

    #[test]
    fn ensure_adds_only_what_is_missing() {
        let base = vec!["latest".into(), "latest-1".into(), "running".into()];
        let mut mv = Multiversion::parse("latest,running");

        assert!(mv.ensure(&base, &[Evr::parse("7.0.12-1.1")]));
        assert_eq!(mv.render(), "latest,running,latest-1,7.0.12-1.1");

        // Idempotent: a second pass changes nothing.
        assert!(!mv.ensure(&base, &[Evr::parse("7.0.12-1.1")]));
    }

    #[test]
    fn unpin_removes_versions_but_never_keywords() {
        let mut mv = Multiversion::parse("latest,latest-1,running,7.0.12-1.1");
        assert!(mv.unpin(&Evr::parse("7.0.12-1.1")));
        assert_eq!(mv.render(), "latest,latest-1,running");
        assert!(!mv.unpin(&Evr::parse("7.0.12-1.1")));
    }

    #[test]
    fn finds_the_setting_and_ignores_comments() {
        let text = "## multiversion.kernels = latest\nmultiversion = provides:multiversion(kernel)\nmultiversion.kernels = latest,running\n";
        let (idx, value) = find_multiversion(text).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(value, "latest,running");
    }

    #[test]
    fn rewrite_preserves_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("zypp.conf");
        std::fs::write(
            &conf,
            "# a comment\n[main]\nmultiversion = provides:multiversion(kernel)\nmultiversion.kernels = latest,running\nother.setting = keep-me\n",
        )
        .unwrap();

        let mut mv = read_multiversion(&conf).unwrap().unwrap();
        mv.ensure(&["latest-1".into()], &[Evr::parse("7.0.12-1.1")]);
        assert!(write_multiversion(&conf, &mv, false).unwrap());

        let text = std::fs::read_to_string(&conf).unwrap();
        assert!(text.contains("multiversion.kernels = latest,running,latest-1,7.0.12-1.1"));
        assert!(text.contains("other.setting = keep-me"));
        assert!(text.contains("# a comment"));
        assert!(text.ends_with('\n'));

        // Writing the same value again is a no-op.
        assert!(!write_multiversion(&conf, &mv, false).unwrap());
    }

    #[test]
    fn refuses_to_invent_a_missing_setting() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("zypp.conf");
        std::fs::write(&conf, "[main]\n").unwrap();
        let err = write_multiversion(&conf, &Multiversion::parse("latest"), false).unwrap_err();
        assert!(err.to_string().contains("refusing to invent"));
    }
}
