//! First-run setup, and its reverse.
//!
//! `sluice setup` looks at the machine, asks a few questions with sensible
//! defaults, shows the whole plan, and then — after one password prompt —
//! installs itself, writes a configuration fitted to this machine, installs
//! the daily check, takes over a manual kernel lock, and records the install
//! and boot history. Everything it does can be undone with `sluice uninstall`
//! (and the lock takeover with `sluice migrate --undo`).
//!
//! The interactive part runs as the user. The privileged part is the same
//! binary re-run under sudo with the plan it was shown, so nothing happens as
//! root that was not on screen first.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::backend::zypper::Zypper;
use crate::backend::{LockSpec, PackageBackend};
use crate::cli::Style;
use crate::config::{BootConfig, Config};
use crate::exec::{Cmd, Runner};
use crate::version::Evr;

pub const INSTALL_PATH: &str = "/usr/local/bin/sluice";
pub const CONFIG_PATH: &str = "/etc/sluice/config.toml";
pub const WEBHOOK_PATH: &str = "/etc/sluice/webhook";
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// The systemd units, compiled in so a lone binary can set itself up.
pub const UNITS: &[(&str, &str)] = &[
    (
        "sluice-check.service",
        include_str!("../systemd/sluice-check.service"),
    ),
    (
        "sluice-check.timer",
        include_str!("../systemd/sluice-check.timer"),
    ),
    (
        "sluice-update.service",
        include_str!("../systemd/sluice-update.service"),
    ),
    (
        "sluice-update.timer",
        include_str!("../systemd/sluice-update.timer"),
    ),
];

/// Kernel flavours openSUSE ships, most common first.
const KERNEL_FLAVOURS: &[&str] = &[
    "kernel-default",
    "kernel-longterm",
    "kernel-preempt",
    "kernel-rt",
    "kernel-vanilla",
];

// ---------------------------------------------------------------------------
// What is on the machine
// ---------------------------------------------------------------------------

/// A GPU driver bound on this machine, and what goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    pub driver: String,
    /// The firmware package that matters for it, if installed.
    pub firmware: Option<String>,
    /// A changelog regex for the lineage view.
    pub highlight: Option<(String, String)>,
}

impl Gpu {
    fn for_driver(driver: &str, installed: &BTreeSet<String>) -> Option<Self> {
        let (firmware, highlight) = match driver {
            "amdgpu" | "radeon" => (
                "kernel-firmware-amdgpu",
                ("amdgpu", "drm/amd|amdgpu|amdkfd|radeon"),
            ),
            "i915" | "xe" => ("kernel-firmware-i915", ("intel-gpu", "drm/i915|drm/xe")),
            "nouveau" => ("kernel-firmware-nvidia", ("nouveau", "drm/nouveau")),
            _ => return None,
        };
        Some(Gpu {
            driver: driver.to_string(),
            firmware: installed.contains(firmware).then(|| firmware.to_string()),
            highlight: Some((highlight.0.to_string(), highlight.1.to_string())),
        })
    }
}

/// Everything setup needs to know about the machine.
#[derive(Debug, Clone, Default)]
pub struct System {
    pub os_id: Option<String>,
    pub os_name: Option<String>,
    pub zypper: bool,
    pub sdbootutil: bool,
    pub kernel_anchor: Option<String>,
    pub kernels: Vec<Evr>,
    pub running_kernel: Option<String>,
    pub mesa: Option<Evr>,
    pub mesa_sources: Vec<String>,
    pub gpus: Vec<Gpu>,
    pub esp_total_mb: Option<u64>,
    pub esp_free_mb: Option<u64>,
    /// Blanket locks on kernel packages: a manual setup to adopt.
    pub blanket_locks: Vec<String>,
    pub multiversion: Option<String>,
    pub installed_version: Option<String>,
    pub existing_config: Option<PathBuf>,
    pub graphical: bool,
    /// How each kernel has behaved here, by upstream version, where known.
    pub kernel_records: BTreeMap<String, crate::evidence::KernelRecord>,
}

impl System {
    pub fn tumbleweed(&self) -> bool {
        self.os_id
            .as_deref()
            .is_some_and(|id| id.starts_with("opensuse"))
    }
}

fn os_release() -> (Option<String>, Option<String>) {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(|v| v.trim_matches('"').to_string())
    };
    (get("ID"), get("PRETTY_NAME"))
}

/// Look at the machine. Needs no privileges and changes nothing.
pub fn detect(r: &mut Runner) -> System {
    let (os_id, os_name) = os_release();
    let exists = |p: &str| Path::new(p).exists();
    let zypper_backend = Zypper::default();

    let flavours: Vec<String> = KERNEL_FLAVOURS.iter().map(|s| s.to_string()).collect();
    let kernels_installed = zypper_backend
        .installed_details(r, &flavours)
        .unwrap_or_default();
    let kernel_anchor = KERNEL_FLAVOURS
        .iter()
        .find(|f| kernels_installed.iter().any(|p| p.name == **f))
        .map(|s| s.to_string());
    let mut kernels: Vec<Evr> = kernels_installed
        .iter()
        .filter(|p| Some(&p.name) == kernel_anchor.as_ref())
        .map(|p| p.evr.clone())
        .collect();
    kernels.sort();

    // Mesa, and every source package built at its version (Mesa-drivers),
    // but not unrelated ones that merely share the name (Mesa-demo).
    let all = zypper_backend.installed_details(r, &[]).unwrap_or_default();
    let installed_names: BTreeSet<String> = all.iter().map(|p| p.name.clone()).collect();
    let mesa = all.iter().find(|p| p.name == "Mesa").map(|p| p.evr.clone());
    let mut mesa_sources: Vec<String> = match &mesa {
        Some(m) => all
            .iter()
            .filter(|p| p.evr.version == m.version)
            .filter_map(|p| p.source.clone())
            .filter(|s| s == "Mesa" || s.starts_with("Mesa-"))
            .collect(),
        None => Vec::new(),
    };
    mesa_sources.sort();
    mesa_sources.dedup();

    let bound = crate::timeline::bound_modules();
    let gpus: Vec<Gpu> = ["amdgpu", "radeon", "i915", "xe", "nouveau"]
        .iter()
        .filter(|d| bound.contains(**d))
        .filter_map(|d| Gpu::for_driver(d, &installed_names))
        .collect();

    let esp = crate::boot::esp_status(&BootConfig::default(), r)
        .ok()
        .flatten();
    let blanket_locks = zypper_backend
        .locks(r)
        .unwrap_or_default()
        .into_iter()
        .filter(|l| {
            l.is_blanket()
                && l.name.starts_with("kernel-")
                && !l.name.starts_with("kernel-firmware")
        })
        .map(|l| l.name)
        .collect();
    let multiversion = crate::boot::read_multiversion(&BootConfig::default().package_manager_conf)
        .ok()
        .flatten()
        .map(|m| m.render());

    let installed_version = Path::new(INSTALL_PATH).exists().then(|| {
        std::process::Command::new(INSTALL_PATH)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown version".into())
    });
    let existing_config = Config::candidates().into_iter().find(|p| p.is_file());

    // How each kernel has behaved, as far as can be told without root: it
    // informs the default known-good choice, never decides it.
    let mut kernel_records = BTreeMap::new();
    if let (Some(anchor), Ok(mut report)) = (
        kernel_anchor.as_deref(),
        crate::health::report(&Default::default(), r),
    ) {
        let evidence = crate::evidence::Evidence::load(Path::new("/var/lib/sluice"));
        let installed_now: Vec<(Evr, Option<chrono::DateTime<Utc>>)> = kernels_installed
            .iter()
            .filter(|p| p.name == anchor)
            .map(|p| (p.evr.clone(), p.installed_at))
            .collect();
        let running = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string());
        crate::evidence::attribute(
            &mut report,
            &evidence,
            anchor,
            running.as_deref(),
            &installed_now,
        );
        for (v, rec) in crate::evidence::per_version(&report.boots) {
            kernel_records.insert(crate::timeline::normalize(&v), rec);
        }
    }

    System {
        os_id,
        os_name,
        zypper: exists("/usr/bin/zypper"),
        sdbootutil: exists("/usr/bin/sdbootutil") || exists("/usr/sbin/sdbootutil"),
        kernel_anchor,
        kernels,
        running_kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string()),
        mesa,
        mesa_sources,
        gpus,
        esp_total_mb: esp.as_ref().map(|e| e.total_mb),
        esp_free_mb: esp.as_ref().map(|e| e.free_mb),
        blanket_locks,
        multiversion,
        installed_version,
        existing_config,
        graphical: std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some(),
        kernel_records,
    }
}

// ---------------------------------------------------------------------------
// The answers, and the configuration they produce
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    pub gate_kernel: bool,
    pub track_gpu_stack: bool,
    pub desktop_notifications: bool,
    pub webhook: Option<String>,
}

fn webhook_format(url: &str) -> &'static str {
    if url.contains("discord.com/") || url.contains("discordapp.com/") {
        "discord"
    } else if url.contains("hooks.slack.com/") {
        "slack"
    } else {
        "raw"
    }
}

/// A configuration fitted to this machine, commented so it can be read and
/// edited. Only what differs from the defaults is written.
pub fn render_config(sys: &System, c: &Choices) -> String {
    let mut out = String::new();
    let w = |out: &mut String, s: &str| {
        out.push_str(s);
        out.push('\n');
    };
    w(
        &mut out,
        &format!(
            "# Written by `sluice setup` on {}. Edit freely: `sluice show-config`\n\
         # prints what is in effect, defaults included, and `sluice setup` can be\n\
         # run again at any time.",
            Utc::now().format("%Y-%m-%d")
        ),
    );

    // Boot.
    let small_esp = sys.esp_total_mb.is_some_and(|t| t < 2048);
    if small_esp || sys.sdbootutil {
        w(&mut out, "\n[boot]");
        if small_esp {
            w(
                &mut out,
                "# The ESP is small, so warn while there is still room for one more kernel.",
            );
            w(&mut out, "esp_warn_free_mb = 256");
        }
        if sys.sdbootutil {
            w(&mut out, "manager = \"sdbootutil\"");
        }
    }

    // Notifications.
    w(&mut out, "\n[notify]");
    if let Some(url) = &c.webhook {
        w(
            &mut out,
            "# The URL itself lives in this root-only file, never in configuration.",
        );
        w(&mut out, &format!("webhook_file = \"{WEBHOOK_PATH}\""));
        w(
            &mut out,
            &format!("webhook_format = \"{}\"", webhook_format(url)),
        );
    }
    w(&mut out, &format!("desktop = {}", c.desktop_notifications));

    // The kernel.
    if let (true, Some(anchor)) = (c.gate_kernel, &sys.kernel_anchor) {
        w(&mut out, "\n[component.kernel]");
        w(
            &mut out,
            "# Fixes inside the running series flow; a new series waits for `sluice promote`.",
        );
        w(&mut out, "policy = \"hold-series\"");
        w(&mut out, "packages = \"auto\"");
        w(&mut out, &format!("anchor = \"{anchor}\""));
        w(&mut out, "include_kmps = true");
        w(
            &mut out,
            "# A hint only: .0 and .1 of a series are where regressions tend to cluster.",
        );
        w(&mut out, "promote_hint_min_point = 3");
        w(&mut out, "lineage = \"linux-stable\"");
        w(&mut out, "boot_entries = true");
        let highlights: Vec<&(String, String)> = sys
            .gpus
            .iter()
            .filter_map(|g| g.highlight.as_ref())
            .collect();
        if !highlights.is_empty() {
            w(&mut out, "\n[component.kernel.highlight]");
            w(
                &mut out,
                "# Counted in each release's changelog: the subsystems this machine leans on.",
            );
            for (label, re) in highlights {
                w(&mut out, &format!("{label} = '{re}'"));
            }
        }
    }

    // The GPU stack.
    let mut stack: Vec<String> = Vec::new();
    if c.gate_kernel && sys.kernel_anchor.is_some() {
        stack.push("kernel".into());
    }
    if c.track_gpu_stack {
        if sys.mesa.is_some() {
            w(&mut out, "\n[component.mesa]");
            w(&mut out, "# Tracked with its history on the timeline; `policy = \"hold-series\"` gates it too.");
            w(&mut out, "policy = \"follow\"");
            w(&mut out, "packages = \"auto\"");
            w(&mut out, "anchor = \"Mesa\"");
            if !sys.mesa_sources.is_empty() {
                let list: Vec<String> = sys
                    .mesa_sources
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect();
                w(&mut out, &format!("family_sources = [{}]", list.join(", ")));
            }
            w(&mut out, "lineage = \"mesa\"");
            stack.push("mesa".into());
        }
        for fw in sys.gpus.iter().filter_map(|g| g.firmware.as_ref()) {
            let name = format!("{}-firmware", fw.trim_start_matches("kernel-firmware-"));
            if stack.contains(&name) {
                continue;
            }
            w(&mut out, &format!("\n[component.{name}]"));
            w(
                &mut out,
                "# Date-versioned: there is no series to hold. `policy = \"soak\"` waits a week.",
            );
            w(&mut out, "policy = \"follow\"");
            w(&mut out, &format!("packages = [\"{fw}\"]"));
            w(&mut out, &format!("anchor = \"{fw}\""));
            w(&mut out, "lineage = \"linux-firmware\"");
            stack.push(name);
        }
        if stack.len() > 1 {
            w(&mut out, "\n[bundle.gpu]");
            w(
                &mut out,
                "# Promoted, marked good and rolled back together: `sluice promote gpu`.",
            );
            let list: Vec<String> = stack.iter().map(|s| format!("\"{s}\"")).collect();
            w(&mut out, &format!("components = [{}]", list.join(", ")));
        }
    }
    out
}

/// The kernel to suggest as the known-good fallback.
///
/// Evidence ranks, in order: a clean record here (a day or more of uptime and
/// no unclean ends), then no record at all, then a record with unclean ends —
/// a kernel known to have frozen this machine is never the first suggestion
/// while another is not known to have. Ties go to the newest.
pub fn suggest_known_good(sys: &System) -> Option<Evr> {
    let rank = |k: &Evr| -> (u8, i64) {
        match sys
            .kernel_records
            .get(&crate::timeline::normalize(&k.version))
        {
            Some(r) if r.unclean == 0 && r.hours >= 24.0 => (0, 0),
            None => (1, 0),
            Some(r) if r.unclean == 0 => (1, 0),
            Some(r) => (2, (r.rate().unwrap_or(f64::MAX) * 1000.0) as i64),
        }
    };
    sys.kernels
        .iter()
        .min_by(|a, b| rank(a).cmp(&rank(b)).then(b.cmp(a)))
        .cloned()
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// Everything the privileged half will do, exactly as shown to the user.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The binary to install (the one running setup).
    pub binary_from: PathBuf,
    pub install_binary: bool,
    /// New configuration text, or `None` to keep what is there.
    pub config: Option<String>,
    pub webhook: Option<String>,
    pub check_timer: bool,
    pub update_timer: bool,
    /// Adopt manual locks, recording this as known-good.
    pub migrate: Option<Evr>,
    pub record_evidence: bool,
}

impl Plan {
    pub fn steps(&self) -> Vec<String> {
        let mut s = Vec::new();
        if self.install_binary {
            s.push(format!("install sluice to {INSTALL_PATH}"));
        }
        if self.config.is_some() {
            s.push(format!(
                "write {CONFIG_PATH}{}",
                if Path::new(CONFIG_PATH).exists() {
                    " (the current one is kept as a .bak)"
                } else {
                    ""
                }
            ));
        }
        if self.webhook.is_some() {
            s.push(format!(
                "store the webhook URL in {WEBHOOK_PATH}, readable by root only"
            ));
        }
        if self.check_timer {
            s.push("install and start sluice-check.timer: a daily check that notifies you".into());
        }
        if self.update_timer {
            s.push("install and start sluice-update.timer: a weekly automatic update".into());
        }
        if let Some(kg) = &self.migrate {
            s.push(format!(
                "take over the kernel lock: fixes flow, new series wait; {kg} is the known-good fallback \
                 (reversible: `sluice migrate --undo`)"
            ));
        }
        if self.record_evidence {
            s.push("record install history and which kernel each boot ran".into());
        }
        s
    }
}

// ---------------------------------------------------------------------------
// The interactive half
// ---------------------------------------------------------------------------

struct Prompt {
    yes: bool,
}

impl Prompt {
    fn line(&self, question: &str, default: &str) -> Result<String> {
        if self.yes {
            return Ok(default.to_string());
        }
        print!("{question} ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let answer = line.trim();
        Ok(if answer.is_empty() {
            default.to_string()
        } else {
            answer.to_string()
        })
    }

    fn yes_no(&self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let a = self.line(
            &format!("{question} {hint}"),
            if default { "y" } else { "n" },
        )?;
        Ok(matches!(a.to_ascii_lowercase().as_str(), "y" | "yes"))
    }
}

/// `sluice setup`.
pub fn run(yes: bool, style: Style) -> Result<i32> {
    anyhow::ensure!(
        yes || std::io::stdin().is_terminal(),
        "setup asks a few questions; run it in a terminal, or pass --yes to accept every default"
    );
    let p = Prompt { yes };
    let mut r = Runner::new(false, None);

    println!();
    println!("  {}", style.bold(&style.cyan("sluice")));
    println!(
        "  {}",
        style.dim("series-aware update gating — fixes flow, new series wait for you")
    );
    println!();
    println!("{}", style.dim("Looking at this machine…"));
    let sys = detect(&mut r);

    let ok = |s: &str| println!("  {} {s}", style.green("✔"));
    let warn = |s: &str| println!("  {} {s}", style.yellow("!"));
    let bad = |s: &str| println!("  {} {s}", style.red("✖"));

    match (&sys.os_name, sys.tumbleweed()) {
        (Some(n), true) => ok(n),
        (Some(n), false) => warn(&format!(
            "{n} — sluice is built for openSUSE; expect rough edges"
        )),
        (None, _) => warn("unknown distribution"),
    }
    if !sys.zypper {
        bad("zypper not found — sluice manages zypper systems");
        return Ok(1);
    }
    ok(if sys.sdbootutil {
        "zypper · systemd-boot via sdbootutil"
    } else {
        "zypper"
    });
    match &sys.kernel_anchor {
        Some(a) => ok(&format!(
            "{a}: {}{}",
            sys.kernels
                .iter()
                .map(Evr::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            sys.running_kernel
                .as_deref()
                .map(|k| format!(" (running {k})"))
                .unwrap_or_default()
        )),
        None => warn("no openSUSE kernel package found"),
    }
    for l in &sys.blanket_locks {
        warn(&format!(
            "a manual lock holds {l} back — sluice can replace it with a series gate"
        ));
    }
    for g in &sys.gpus {
        let mut s = format!("GPU on {}", g.driver);
        if let Some(m) = &sys.mesa {
            s.push_str(&format!(" · Mesa {m}"));
        }
        if let Some(f) = &g.firmware {
            s.push_str(&format!(" · {f}"));
        }
        ok(&s);
    }
    if let (Some(free), Some(total)) = (sys.esp_free_mb, sys.esp_total_mb) {
        let line = format!("ESP {free} MB free of {total} MB");
        if free < 256 {
            warn(&line);
        } else {
            ok(&line);
        }
    }
    if let Some(v) = &sys.installed_version {
        ok(&format!(
            "{v} is already installed at {INSTALL_PATH} — this will update it"
        ));
    }

    println!();
    println!(
        "{}",
        style.dim("A few questions. Enter accepts the default.")
    );
    println!();

    let reconfigure = match &sys.existing_config {
        Some(path) => p.yes_no(
            &format!("A configuration exists at {}. Replace it?", path.display()),
            false,
        )?,
        None => true,
    };

    let mut choices = Choices {
        gate_kernel: true,
        track_gpu_stack: !sys.gpus.is_empty(),
        desktop_notifications: sys.graphical,
        webhook: None,
    };
    if reconfigure {
        if sys.kernel_anchor.is_some() {
            choices.gate_kernel = p.yes_no(
                "Gate the kernel by series — fixes install automatically, a new series waits for you?",
                true,
            )?;
        }
        if !sys.gpus.is_empty() {
            choices.track_gpu_stack = p.yes_no(
                "Track Mesa and GPU firmware too (their history on the timeline, gated later if you like)?",
                true,
            )?;
        }
        choices.desktop_notifications = p.yes_no("Desktop notifications?", sys.graphical)?;
        let url = p.line(
            "Webhook URL for notifications (Discord or Slack; stored root-only; Enter to skip):",
            "",
        )?;
        if !url.is_empty() {
            anyhow::ensure!(
                url.starts_with("https://"),
                "a webhook URL must start with https://"
            );
            choices.webhook = Some(url);
        }
    }

    let check_timer = p.yes_no(
        "Run a daily check that tells you about new series and problems?",
        true,
    )?;
    let update_timer = p.yes_no(
        "Apply the updates your policies allow automatically, weekly?",
        false,
    )?;

    let mut migrate = None;
    if choices.gate_kernel && !sys.kernels.is_empty() {
        let suggested = suggest_known_good(&sys);
        println!();
        println!("Which installed kernel is your known-good fallback — the one to go back to?");
        for (i, k) in sys.kernels.iter().enumerate() {
            let rate = match sys
                .kernel_records
                .get(&crate::timeline::normalize(&k.version))
            {
                Some(r) if r.unclean > 0 => style.red(&format!("  {}", r.summary())),
                Some(r) => style.green(&format!("  {}", r.summary())),
                None => style.dim("  no record on this machine yet"),
            };
            let mark = if Some(k) == suggested.as_ref() {
                style.green(" (suggested)")
            } else {
                String::new()
            };
            println!("  {}) {k}{rate}{mark}", i + 1);
        }
        let default_index = suggested
            .as_ref()
            .and_then(|s| sys.kernels.iter().position(|k| k == s))
            .map_or(1, |i| i + 1);
        let answer = p.line(
            &format!("choice [{default_index}]:"),
            &default_index.to_string(),
        )?;
        let index: usize = answer
            .parse()
            .ok()
            .filter(|n| (1..=sys.kernels.len()).contains(n))
            .with_context(|| format!("`{answer}` is not one of the choices"))?;
        let chosen = sys.kernels[index - 1].clone();
        let adopt = !sys.blanket_locks.is_empty() || sys.installed_version.is_none();
        if adopt
            && p.yes_no(
                "Take over the kernel lock now? (reversible with `sluice migrate --undo`)",
                true,
            )?
        {
            migrate = Some(chosen);
        }
    }

    let plan = Plan {
        binary_from: std::env::current_exe()?,
        install_binary: true,
        config: reconfigure.then(|| render_config(&sys, &choices)),
        webhook: choices.webhook.clone(),
        check_timer,
        update_timer,
        migrate,
        record_evidence: true,
    };

    println!();
    println!("{}", style.bold("The plan"));
    for s in plan.steps() {
        println!("  • {s}");
    }
    println!();
    if !p.yes_no("Go ahead? This asks for your password once.", true)? {
        println!("{}", style.dim("Nothing was changed."));
        return Ok(1);
    }

    // Hand the plan to a privileged copy of this binary.
    let dir = tempfile_dir()?;
    let plan_path = dir.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?)?;
    let status = if crate::privilege::is_root() {
        apply(&plan_path, style).map(|_| true)?
    } else {
        let escalator = if which("sudo") { "sudo" } else { "pkexec" };
        std::process::Command::new(escalator)
            .arg(&plan.binary_from)
            .arg("setup")
            .arg("--apply")
            .arg(&plan_path)
            .status()
            .with_context(|| format!("running {escalator}"))?
            .success()
    };
    let _ = std::fs::remove_dir_all(&dir);
    if !status {
        println!(
            "{}",
            style.red("Setup did not finish; see above. Running it again is safe.")
        );
        return Ok(1);
    }

    println!();
    println!("{}", style.green("sluice is set up."));
    println!("  {}   open the dashboard", style.bold("sluice"));
    println!(
        "  {}   what gates what, right now",
        style.bold("sluice status")
    );
    println!(
        "  {}   everything setup did, undone",
        style.bold("sluice uninstall")
    );
    println!();
    if std::io::stdin().is_terminal() && !yes && p.yes_no("Open the dashboard now?", true)? {
        let err = exec(INSTALL_PATH);
        anyhow::bail!("could not start the dashboard: {err}");
    }
    Ok(0)
}

fn tempfile_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir().join(format!("sluice-setup-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(base)
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

#[cfg(unix)]
fn exec(path: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(path).exec()
}

// ---------------------------------------------------------------------------
// The privileged half
// ---------------------------------------------------------------------------

/// `sluice setup --apply PLAN`: carry out a plan the user has already seen.
pub fn apply(plan_path: &Path, style: Style) -> Result<()> {
    anyhow::ensure!(
        crate::privilege::is_root(),
        "applying a setup plan needs root"
    );
    let plan: Plan = serde_json::from_str(
        &std::fs::read_to_string(plan_path)
            .with_context(|| format!("reading {}", plan_path.display()))?,
    )
    .context("reading the setup plan")?;
    let mut r = Runner::new(false, Some(PathBuf::from("/var/log/sluice.log")));

    let step = |what: &str, result: Result<()>| -> Result<()> {
        match result {
            Ok(()) => {
                println!("  {} {what}", style.green("✔"));
                Ok(())
            }
            Err(e) => {
                println!("  {} {what}: {e:#}", style.red("✖"));
                Err(e)
            }
        }
    };

    if plan.install_binary && plan.binary_from != Path::new(INSTALL_PATH) {
        step(
            &format!("installed {INSTALL_PATH}"),
            install_file(&plan.binary_from, Path::new(INSTALL_PATH), 0o755),
        )?;
    }
    if let Some(text) = &plan.config {
        step(&format!("wrote {CONFIG_PATH}"), write_config(text))?;
    }
    if let Some(url) = &plan.webhook {
        step(
            &format!("stored the webhook in {WEBHOOK_PATH}"),
            write_private(Path::new(WEBHOOK_PATH), &format!("{url}\n")),
        )?;
    }

    let mut timers = Vec::new();
    if plan.check_timer {
        timers.push("sluice-check.timer");
    }
    if plan.update_timer {
        timers.push("sluice-update.timer");
    }
    if !timers.is_empty() {
        step("installed the systemd units", install_units(&mut r))?;
        for t in &timers {
            let _ = step(
                &format!("enabled {t}"),
                r.run(&Cmd::mutate("systemctl").arg("enable").arg("--now").arg(t))
                    .and_then(|o| o.require_ok("systemctl enable").map(|_| ())),
            );
        }
    }

    // The rest runs against the configuration just written.
    let config = Config::load(Path::new(CONFIG_PATH)).or_else(|_| Config::discover(None))?;
    let mut app = crate::app::App::new(config, false)?;
    if let Some(kg) = &plan.migrate {
        if app.state.migration.is_none() {
            let choices: BTreeMap<String, Evr> = [("kernel".to_string(), kg.clone())].into();
            let result = app.migrate(&choices, Utc::now());
            let _ = step(
                "took over the kernel lock",
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{e:#}")),
            );
            if let Ok(report) = result {
                for c in report.lock_changes {
                    println!("      {}", style.dim(&c));
                }
                for n in report.notes {
                    println!("      {}", style.dim(&n));
                }
            }
        } else {
            println!(
                "  {} the kernel lock was already taken over earlier",
                style.dim("·")
            );
        }
    }
    if plan.record_evidence {
        let _ = step(
            "recorded install history and boot evidence",
            app.health_report().map(|_| ()),
        );
    }
    Ok(())
}

fn install_file(from: &Path, to: &Path, mode: u32) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = to.with_extension("sluice-new");
    std::fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(&tmp, to).with_context(|| format!("replacing {}", to.display()))?;
    Ok(())
}

fn write_config(text: &str) -> Result<()> {
    let path = Path::new(CONFIG_PATH);
    // Refuse to write something that would not load.
    let tmp = std::env::temp_dir().join(format!("sluice-config-check-{}.toml", std::process::id()));
    std::fs::write(&tmp, text)?;
    let check = Config::load(&tmp);
    let _ = std::fs::remove_file(&tmp);
    check.context("the generated configuration does not load")?;

    if path.exists() {
        let backup =
            path.with_extension(format!("toml.bak-{}", Utc::now().format("%Y%m%d-%H%M%S")));
        std::fs::copy(path, &backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn install_units(r: &mut Runner) -> Result<()> {
    for (name, text) in UNITS {
        std::fs::write(Path::new(UNIT_DIR).join(name), text)?;
    }
    r.run(&Cmd::mutate("systemctl").arg("daemon-reload"))?
        .require_ok("systemctl daemon-reload")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

/// `sluice uninstall`: undo what setup did. Configuration, state and the
/// vault are kept unless `purge` is set, so a reinstall picks up where it
/// left off.
pub fn uninstall(purge: bool, style: Style) -> Result<i32> {
    anyhow::ensure!(
        crate::privilege::is_root(),
        "`sluice uninstall` changes the system; run it with sudo"
    );
    let mut r = Runner::new(false, Some(PathBuf::from("/var/log/sluice.log")));
    let say = |ok: bool, s: &str| {
        println!(
            "  {} {s}",
            if ok {
                style.green("✔")
            } else {
                style.yellow("·")
            }
        );
    };

    for timer in ["sluice-check.timer", "sluice-update.timer"] {
        let _ = r.run(
            &Cmd::mutate("systemctl")
                .arg("disable")
                .arg("--now")
                .arg(timer),
        );
    }
    let mut removed_units = false;
    for (name, _) in UNITS {
        let path = Path::new(UNIT_DIR).join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
            removed_units = true;
        }
    }
    if removed_units {
        let _ = r.run(&Cmd::mutate("systemctl").arg("daemon-reload"));
        say(true, "stopped and removed the timers");
    }

    // Give the locks and zypp.conf back exactly as they were.
    if let Ok(config) = Config::discover(None) {
        if let Ok(mut app) = crate::app::App::new(config, false) {
            if app.state.migration.is_some() {
                match app.migrate_undo(Utc::now()) {
                    Ok(_) => say(true, "restored the locks and settings from before sluice"),
                    Err(e) => say(false, &format!("could not undo the lock takeover: {e:#}")),
                }
            } else {
                // Remove any gate sluice added outside a migration.
                let locks: Vec<LockSpec> = app.backend.locks(&mut app.runner).unwrap_or_default();
                for l in locks.iter().filter(|l| {
                    l.comment
                        .as_deref()
                        .is_some_and(|c| c.starts_with("sluice:"))
                }) {
                    let _ = app.backend.remove_lock(&mut app.runner, l);
                }
            }
        }
    }

    if purge {
        for path in ["/etc/sluice", "/var/lib/sluice", "/var/cache/sluice"] {
            if Path::new(path).exists() {
                std::fs::remove_dir_all(path)?;
            }
        }
        say(true, "removed configuration, state, cache and the vault");
    } else {
        say(false, "kept /etc/sluice, /var/lib/sluice (state and vault) and the cache; --purge removes them");
    }

    if Path::new(INSTALL_PATH).exists() {
        std::fs::remove_file(INSTALL_PATH)?;
        say(true, &format!("removed {INSTALL_PATH}"));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amd_workstation() -> System {
        System {
            os_id: Some("opensuse-tumbleweed".into()),
            zypper: true,
            sdbootutil: true,
            kernel_anchor: Some("kernel-default".into()),
            kernels: vec![Evr::parse("7.0.12-1.1"), Evr::parse("7.2.0-1.1")],
            running_kernel: Some("7.2.0-1-default".into()),
            mesa: Some(Evr::parse("26.2.2-2.1")),
            mesa_sources: vec!["Mesa".into(), "Mesa-drivers".into()],
            gpus: vec![
                Gpu::for_driver("amdgpu", &["kernel-firmware-amdgpu".to_string()].into()).unwrap(),
            ],
            esp_total_mb: Some(1021),
            esp_free_mb: Some(182),
            ..Default::default()
        }
    }

    fn load(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path).unwrap_or_else(|e| panic!("{e:#}\n---\n{text}"))
    }

    #[test]
    fn a_generated_configuration_loads_and_fits_the_machine() {
        let choices = Choices {
            gate_kernel: true,
            track_gpu_stack: true,
            desktop_notifications: true,
            webhook: Some("https://discord.com/api/webhooks/0/example".into()),
        };
        let text = render_config(&amd_workstation(), &choices);
        let cfg = load(&text);

        let kernel = cfg.component("kernel").unwrap();
        assert_eq!(kernel.anchor.as_deref(), Some("kernel-default"));
        assert!(kernel.boot_entries);
        assert!(kernel.highlight.contains_key("amdgpu"));
        assert_eq!(
            cfg.component("mesa").unwrap().family_sources,
            vec!["Mesa", "Mesa-drivers"]
        );
        assert!(cfg.components.contains_key("amdgpu-firmware"));
        assert_eq!(cfg.bundle("gpu").unwrap().components.len(), 3);
        assert_eq!(
            cfg.boot.esp_warn_free_mb, 256,
            "a small ESP gets an early warning"
        );
        assert_eq!(
            cfg.notify.webhook_file.as_deref(),
            Some(Path::new(WEBHOOK_PATH))
        );
        assert!(
            !text.contains("discord.com/api"),
            "the URL is never written into configuration"
        );
    }

    #[test]
    fn a_minimal_machine_gets_a_minimal_configuration() {
        let sys = System {
            os_id: Some("opensuse-tumbleweed".into()),
            zypper: true,
            kernel_anchor: Some("kernel-default".into()),
            kernels: vec![Evr::parse("7.2.0-1.1")],
            ..Default::default()
        };
        let choices = Choices {
            gate_kernel: true,
            track_gpu_stack: true,
            desktop_notifications: false,
            webhook: None,
        };
        let cfg = load(&render_config(&sys, &choices));
        assert_eq!(cfg.components.len(), 1);
        assert!(cfg.bundles.is_empty(), "no stack to bundle");
        assert!(cfg.notify.webhook_file.is_none());
    }

    fn record(boots: usize, hours: f64, unclean: usize) -> crate::evidence::KernelRecord {
        crate::evidence::KernelRecord {
            boots,
            hours,
            unclean,
            pstore: 0,
            inferred: 0,
        }
    }

    #[test]
    fn a_kernel_known_to_have_frozen_is_never_the_first_suggestion() {
        let mut sys = amd_workstation();
        // Only the kernel that froze has a record; the other is unknown.
        sys.kernel_records
            .insert("7.2.0".into(), record(9, 427.0, 2));
        assert_eq!(suggest_known_good(&sys).unwrap().to_string(), "7.0.12-1.1");

        // A clean record beats an unknown one.
        sys.kernel_records
            .insert("7.2.0".into(), record(9, 427.0, 0));
        assert_eq!(suggest_known_good(&sys).unwrap().to_string(), "7.2.0-1.1");

        // With nothing known, the newest.
        sys.kernel_records.clear();
        assert_eq!(suggest_known_good(&sys).unwrap().to_string(), "7.2.0-1.1");

        // Both froze: the lower rate.
        sys.kernel_records
            .insert("7.2.0".into(), record(9, 427.0, 2));
        sys.kernel_records
            .insert("7.0.12".into(), record(40, 800.0, 1));
        assert_eq!(suggest_known_good(&sys).unwrap().to_string(), "7.0.12-1.1");
    }

    #[test]
    fn the_embedded_units_run_the_installed_binary() {
        for (name, text) in UNITS {
            if name.ends_with(".service") {
                assert!(
                    text.contains(&format!("ExecStart={INSTALL_PATH} ")),
                    "{name} must run {INSTALL_PATH}"
                );
            }
        }
    }

    #[test]
    fn webhook_formats_follow_the_host() {
        assert_eq!(
            webhook_format("https://discord.com/api/webhooks/1/x"),
            "discord"
        );
        assert_eq!(
            webhook_format("https://hooks.slack.com/services/example"),
            "slack"
        );
        assert_eq!(webhook_format("https://example.org/hook"), "raw");
    }

    #[test]
    fn the_plan_lists_every_step_it_will_take() {
        let plan = Plan {
            install_binary: true,
            config: Some(String::new()),
            check_timer: true,
            migrate: Some(Evr::parse("7.0.12-1.1")),
            record_evidence: true,
            ..Default::default()
        };
        let steps = plan.steps().join("\n");
        assert!(steps.contains(INSTALL_PATH));
        assert!(steps.contains("sluice-check.timer"));
        assert!(steps.contains("7.0.12-1.1"));
        assert!(steps.contains("migrate --undo"));
    }
}
