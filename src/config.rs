//! Configuration model.
//!
//! Every path, URL and threshold in sluice is configurable. The compiled-in
//! defaults describe a generic rolling-release workstation, not any particular
//! machine; see `config/profiles/` for worked examples.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What sluice does when a component has a newer candidate in the repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// No gating: behaves like a plain distribution upgrade.
    Follow,
    /// Auto-apply within the current series; gate any series change.
    HoldSeries,
    /// Auto-apply once a candidate has sat unchanged in the repo for `soak_days`.
    Soak,
    /// Never update automatically.
    Hold,
}

impl Policy {
    pub fn label(self) -> &'static str {
        match self {
            Policy::Follow => "follow",
            Policy::HoldSeries => "hold-series",
            Policy::Soak => "soak",
            Policy::Hold => "hold",
        }
    }
}

/// How the set of packages belonging to a component is determined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSelector {
    /// `"auto"` — derive siblings of `anchor` from the installed package set,
    /// or a single glob such as `"Mesa*"`.
    Pattern(String),
    /// An explicit list of package names.
    List(Vec<String>),
}

impl PackageSelector {
    pub fn is_auto(&self) -> bool {
        matches!(self, PackageSelector::Pattern(p) if p == "auto")
    }

    /// Glob patterns to query the backend with. `auto` is resolved separately
    /// against the installed set, so it contributes nothing here.
    pub fn patterns(&self) -> Vec<String> {
        match self {
            PackageSelector::Pattern(p) if p == "auto" => Vec::new(),
            PackageSelector::Pattern(p) => vec![p.clone()],
            PackageSelector::List(v) => v.clone(),
        }
    }
}

impl Default for PackageSelector {
    fn default() -> Self {
        PackageSelector::Pattern("auto".into())
    }
}

/// Which upstream lineage provider describes a component's release history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineageSource {
    /// No upstream lineage is available; show repo/changelog data only.
    #[default]
    None,
    /// kernel.org releases.json plus per-release ChangeLogs.
    LinuxStable,
    /// The Mesa release archive plus per-release notes.
    Mesa,
    /// The linux-firmware tarball directory on kernel.org.
    LinuxFirmware,
    /// Any web directory of release tarballs: set `lineage_url` and
    /// `lineage_pattern` (capture group 1 is the version).
    Index,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ComponentConfig {
    pub policy: Policy,
    pub packages: PackageSelector,
    /// The package whose version defines the component's version. Required
    /// when `packages = "auto"`, since that is the root of the family.
    pub anchor: Option<String>,
    /// With `packages = "auto"`, the glob that bounds the family search.
    /// Defaults to the anchor's name up to its first `-`, plus `*`
    /// (`kernel-default` -> `kernel*`).
    pub family_glob: Option<String>,
    /// Group the family by source package instead of by exact version: every
    /// installed package built from one of these sources, at the anchor's
    /// upstream version, is a member whatever its name or release. Mesa needs
    /// this — it is built from `Mesa` and `Mesa-drivers`, whose releases
    /// differ, and it ships `libgbm1` and `libvulkan_radeon` alongside `Mesa-*`.
    pub family_sources: Vec<String>,
    /// Also treat kernel module packages built against the anchor version as
    /// family members. They must move in lockstep or they will not load.
    pub include_kmps: bool,
    /// Capture group 1 of this regex is the series key (e.g. `7.2.6` -> `7.2`).
    pub series_regex: String,
    /// Show a "looks mature" hint at or above this point release. Hint only.
    pub promote_hint_min_point: Option<u32>,
    /// Escalate warnings once the held series has been EOL upstream this long.
    pub warn_after_eol_days: u32,
    /// Days a candidate must sit unchanged before `soak` applies it.
    pub soak_days: u32,
    /// Regexes counted per changelog entry and surfaced in the lineage view.
    /// Empty by default — these encode what *you* care about regressing.
    pub highlight: BTreeMap<String, String>,
    pub lineage: LineageSource,
    /// Overrides the index URL of `mesa`/`linux-firmware`, or sets it for `index`.
    pub lineage_url: Option<String>,
    /// Regex matching release file names in the index; group 1 is the version.
    pub lineage_pattern: Option<String>,
    /// This component has boot entries (a kernel). sluice then keeps the
    /// default boot entry on the version it is holding, re-asserting it after
    /// every transaction, since the distribution's hooks reset it to the
    /// newest installed kernel.
    pub boot_entries: bool,
}

impl Default for ComponentConfig {
    fn default() -> Self {
        Self {
            policy: Policy::Follow,
            packages: PackageSelector::default(),
            anchor: None,
            family_glob: None,
            family_sources: Vec::new(),
            include_kmps: true,
            series_regex: r"^(\d+\.\d+)".into(),
            promote_hint_min_point: None,
            warn_after_eol_days: 14,
            soak_days: 7,
            highlight: BTreeMap::new(),
            lineage: LineageSource::None,
            lineage_url: None,
            lineage_pattern: None,
            boot_entries: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub vault_dir: PathBuf,
    pub log_file: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            state_dir: "/var/lib/sluice".into(),
            cache_dir: "/var/cache/sluice".into(),
            vault_dir: "/var/lib/sluice/vault".into(),
            log_file: "/var/log/sluice.log".into(),
        }
    }
}

impl Paths {
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootManager {
    /// Detect from what is installed on the system.
    #[default]
    Auto,
    Sdbootutil,
    KernelInstall,
    /// Do not attempt to inspect or touch boot entries.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BootConfig {
    pub esp_path: PathBuf,
    /// Warn when the ESP has less than this much free space.
    pub esp_warn_free_mb: u64,
    pub manager: BootManager,
    /// zypp.conf entries sluice guarantees are present alongside pinned versions.
    pub multiversion_base: Vec<String>,
    /// The package manager configuration file holding `multiversion.kernels`.
    pub package_manager_conf: PathBuf,
    /// Where the kernel exposes EFI variables, read for the default entry.
    pub efivars_dir: PathBuf,
}

impl Default for BootConfig {
    fn default() -> Self {
        Self {
            esp_path: "/boot/efi".into(),
            esp_warn_free_mb: 128,
            manager: BootManager::Auto,
            multiversion_base: vec!["latest".into(), "latest-1".into(), "running".into()],
            package_manager_conf: "/etc/zypp/zypp.conf".into(),
            efivars_dir: "/sys/firmware/efi/efivars".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LineageConfig {
    pub releases_url: String,
    pub changelog_base: String,
    pub cache_ttl_hours: u32,
    /// Show at most this many of a series' most recent point releases. Each
    /// is one ChangeLog fetch the first time it is seen.
    pub max_points: usize,
    /// Download a gated candidate's RPM (a kernel is ~100 MB) to show the
    /// distribution's own changelog next to the upstream lineage. The file is
    /// reused when the candidate is later vaulted.
    pub repo_changelog: bool,
    /// Never touch the network; render from cache only.
    pub offline: bool,
    pub http_timeout_secs: u64,
}

impl Default for LineageConfig {
    fn default() -> Self {
        Self {
            releases_url: "https://www.kernel.org/releases.json".into(),
            changelog_base: "https://cdn.kernel.org/pub/linux/kernel".into(),
            cache_ttl_hours: 6,
            max_points: 12,
            repo_changelog: false,
            offline: false,
            http_timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebhookFormat {
    #[default]
    Discord,
    Slack,
    /// POST the message as a bare JSON string body.
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotifyConfig {
    /// Path to a root-only file containing the webhook URL. The URL itself is
    /// never stored in config, so this file is safe to publish.
    pub webhook_file: Option<PathBuf>,
    pub webhook_format: WebhookFormat,
    pub desktop: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            webhook_file: None,
            webhook_format: WebhookFormat::Discord,
            desktop: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    pub journalctl: String,
    /// Directory systemd-pstore archives crash dumps into. Absence of this
    /// directory is normal and is never treated as an error.
    pub pstore_dir: PathBuf,
    /// zypp's transaction log, read (with root) for install history.
    pub zypp_history: PathBuf,
    /// A boot whose journal tail matches any of these ended cleanly. The
    /// defaults are the systemd shutdown sequence; override them if your init
    /// logs something else.
    pub clean_end_patterns: Vec<String>,
    /// How many journal lines from the end of each boot to inspect.
    pub tail_lines: u32,
    /// Ignore boots shorter than this; they are usually installer or rescue
    /// environments rather than real sessions.
    pub min_boot_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            journalctl: "journalctl".into(),
            pstore_dir: "/var/lib/systemd/pstore".into(),
            zypp_history: "/var/log/zypp/history".into(),
            clean_end_patterns: vec![
                r"systemd-shutdown\[1\]:".into(),
                r"Reached target (Shutdown|Power-Off|Power Off|Reboot|Halt|Final Step)".into(),
                r"Shutting down\.".into(),
                r"systemd\[1\]: Finished .*(Power-Off|Reboot|Halt)".into(),
            ],
            tail_lines: 400,
            min_boot_secs: 30,
        }
    }
}

/// Where sluice's own releases come from. sluice never replaces itself: it
/// announces a new release, and `sluice self-update` installs it on request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SelfUpdateConfig {
    /// `owner/name` of the GitHub repository publishing releases.
    pub repo: String,
    /// Look for new releases in `status`, `check` and the dashboard.
    pub check: bool,
    /// Overrides the release API URL (a mirror, or testing).
    pub api_url: Option<String>,
}

impl Default for SelfUpdateConfig {
    fn default() -> Self {
        Self {
            repo: "max-bromberg/sluice".into(),
            check: true,
            api_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    Zypper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackendConfig {
    pub kind: BackendKind,
    /// Refresh repository metadata before evaluating policies.
    pub refresh: bool,
    /// Program used to re-run sluice with privileges from the TUI.
    /// `auto` tries pkexec, then sudo.
    pub escalate_with: String,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            kind: BackendKind::Zypper,
            refresh: true,
            escalate_with: "auto".into(),
        }
    }
}

/// Components promoted, marked good and rolled back together, such as a GPU
/// stack whose kernel driver, userspace and firmware were tested as one. Each
/// member keeps its own policy for everyday updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleConfig {
    pub components: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub paths: Paths,
    pub boot: BootConfig,
    pub health: HealthConfig,
    pub backend: BackendConfig,
    pub lineage: LineageConfig,
    pub notify: NotifyConfig,
    pub self_update: SelfUpdateConfig,
    #[serde(rename = "component")]
    pub components: BTreeMap<String, ComponentConfig>,
    #[serde(rename = "bundle")]
    pub bundles: BTreeMap<String, BundleConfig>,
    /// Set by the loader, not by the file itself.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl Config {
    /// Search order, first hit wins:
    /// `$SLUICE_CONFIG`, `./sluice.toml`, `$XDG_CONFIG_HOME/sluice/config.toml`,
    /// `/etc/sluice/config.toml`. With no file anywhere, compiled-in defaults
    /// are used and `source` is `None`.
    pub fn discover(explicit: Option<&Path>) -> Result<Self> {
        if let Some(p) = explicit {
            return Self::load(p);
        }
        for candidate in Self::candidates() {
            if candidate.is_file() {
                return Self::load(&candidate);
            }
        }
        Ok(Self::default())
    }

    pub fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(env) = std::env::var_os("SLUICE_CONFIG") {
            out.push(PathBuf::from(env));
        }
        out.push(PathBuf::from("sluice.toml"));
        if let Some(cfg) = dirs::config_dir() {
            out.push(cfg.join("sluice/config.toml"));
        }
        out.push(PathBuf::from("/etc/sluice/config.toml"));
        out
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.source = Some(path.to_path_buf());
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (name, c) in &self.components {
            regex::Regex::new(&c.series_regex)
                .with_context(|| format!("component `{name}`: invalid series_regex"))?;
            for (label, pattern) in &c.highlight {
                regex::Regex::new(pattern).with_context(|| {
                    format!("component `{name}`: invalid highlight regex `{label}`")
                })?;
            }
            if let Some(p) = &c.lineage_pattern {
                let re = regex::Regex::new(p)
                    .with_context(|| format!("component `{name}`: invalid lineage_pattern"))?;
                anyhow::ensure!(
                    re.captures_len() >= 2,
                    "component `{name}`: lineage_pattern needs a capture group for the version"
                );
            }
            if c.lineage == LineageSource::Index
                && (c.lineage_url.is_none() || c.lineage_pattern.is_none())
            {
                anyhow::bail!(
                    "component `{name}`: `lineage = \"index\"` needs lineage_url and lineage_pattern"
                );
            }
            if c.packages.is_auto() && c.anchor.is_none() {
                anyhow::bail!(
                    "component `{name}`: `packages = \"auto\"` requires an `anchor` package name"
                );
            }
        }
        for (name, b) in &self.bundles {
            anyhow::ensure!(
                !self.components.contains_key(name),
                "bundle `{name}` has the same name as a component; `sluice promote {name}` would be ambiguous"
            );
            anyhow::ensure!(
                !b.components.is_empty(),
                "bundle `{name}` has no components"
            );
            for m in &b.components {
                anyhow::ensure!(
                    self.components.contains_key(m),
                    "bundle `{name}` names `{m}`, which is not a configured component"
                );
            }
        }
        for p in &self.health.clean_end_patterns {
            regex::Regex::new(p)
                .with_context(|| format!("invalid health.clean_end_patterns entry `{p}`"))?;
        }
        Ok(())
    }

    /// The glob bounding an `auto` family search.
    pub fn family_glob_for(c: &ComponentConfig) -> Option<String> {
        if let Some(g) = &c.family_glob {
            return Some(g.clone());
        }
        let anchor = c.anchor.as_ref()?;
        let stem = anchor.split('-').next().unwrap_or(anchor);
        Some(format!("{stem}*"))
    }

    pub fn bundle(&self, name: &str) -> Result<&BundleConfig> {
        self.bundles
            .get(name)
            .with_context(|| format!("no bundle named `{name}` in config"))
    }

    pub fn component(&self, name: &str) -> Result<&ComponentConfig> {
        self.components
            .get(name)
            .with_context(|| format!("no component named `{name}` in config"))
    }
}
