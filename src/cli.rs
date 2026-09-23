//! Command-line interface: argument parsing and text rendering.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};

use crate::app::{App, CheckReport, MigrateReport, Status, UpdateReport};
use crate::config::Config;
use crate::lineage::LineageView;
use crate::notify::Urgency;
use crate::policy::Decision;
use crate::privilege;
use crate::state::Lifecycle;
use crate::version::Evr;

#[derive(Parser, Debug)]
#[command(
    name = "sluice",
    version,
    about = "Series-aware update gating for rolling distributions",
    long_about = "sluice lets bug-fix releases flow automatically while holding new feature \
                  series behind an explicit decision.\n\n\
                  Run with no arguments to open the interactive dashboard."
)]
pub struct Cli {
    /// Configuration file. Defaults to $SLUICE_CONFIG, ./sluice.toml,
    /// $XDG_CONFIG_HOME/sluice/config.toml, then /etc/sluice/config.toml.
    #[arg(long, short, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Describe what would happen without changing anything.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Never use the network; render upstream data from cache only.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Disable colour even when attached to a terminal.
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open the interactive dashboard (the default with no arguments).
    Tui,
    /// Show each component's version, policy, gate state and evidence.
    Status,
    /// Refresh, apply what policy allows, and report what is gated.
    Update,
    /// Show the upstream release lineage for a component's series.
    Lineage {
        /// Component name, e.g. `kernel`.
        component: String,
        /// A specific series to describe, instead of the gated candidate.
        series: Option<String>,
    },
    /// Cross the gate into a new series. Always explicit, never automatic.
    Promote {
        /// A component, or a bundle to promote as one unit.
        component: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Record a version as known-good: vault it and pin it against purging.
    MarkGood {
        /// A component, or a bundle to mark at its installed versions.
        component: String,
        /// Defaults to the installed version.
        version: Option<String>,
    },
    /// Go back to the known-good version: boot it, and hold its series again.
    Rollback {
        /// A component, or a bundle to roll back together.
        component: String,
        /// Also uninstall the version under test (refused while it is running).
        #[arg(long)]
        remove: bool,
    },
    /// Per-kernel boot evidence: uptime, unclean ends, crash records.
    Health {
        /// Limit to one kernel version.
        kernel: Option<String>,
    },
    /// Non-interactive: notify about anything that needs attention.
    Check {
        /// Re-notify even about things already announced.
        #[arg(long)]
        force: bool,
    },
    /// Adopt an existing manual lock setup.
    Migrate {
        /// Version to record as known-good: `COMPONENT=VERSION`, or a bare
        /// `VERSION` for the component that has it installed. Repeatable.
        /// Without it you are asked, for any component with a choice.
        #[arg(long, value_name = "[COMPONENT=]VERSION")]
        known_good: Vec<String>,
        /// Restore the setup exactly as it was before migration.
        #[arg(long)]
        undo: bool,
    },
    /// Restore locks left off by an interrupted run, and the default boot
    /// entry if something moved it.
    Repair,
    /// Print the effective configuration, including defaults.
    ShowConfig,
    /// First-time setup: install, configure for this machine, adopt locks.
    Setup {
        /// Accept every default without asking.
        #[arg(long, short)]
        yes: bool,
        /// Carry out a plan (used by setup itself, under sudo).
        #[arg(long, hide = true, value_name = "PLAN")]
        apply: Option<PathBuf>,
    },
    /// Install a newer sluice, verified, keeping the current one to go back to.
    SelfUpdate {
        /// Only say whether a newer release is out (exit 1 if so).
        #[arg(long, conflicts_with = "rollback")]
        check: bool,
        /// Put back the version that was replaced.
        #[arg(long)]
        rollback: bool,
        /// Do not ask before installing.
        #[arg(long, short)]
        yes: bool,
        /// Run by the new binary after it is installed.
        #[arg(long, hide = true)]
        post_install: bool,
    },
    /// Undo what setup did: timers, locks, the installed binary.
    Uninstall {
        /// Also remove configuration, state, cache and the vault.
        #[arg(long)]
        purge: bool,
    },
}

impl Command {
    /// Commands that change the system and therefore need root.
    pub fn needs_root(&self) -> bool {
        matches!(
            self,
            Command::Update
                | Command::Promote { .. }
                | Command::MarkGood { .. }
                | Command::Rollback { .. }
                | Command::Migrate { .. }
                | Command::Repair
        ) || matches!(self, Command::SelfUpdate { check: false, .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Command::Tui => "tui",
            Command::Status => "status",
            Command::Update => "update",
            Command::Lineage { .. } => "lineage",
            Command::Promote { .. } => "promote",
            Command::MarkGood { .. } => "mark-good",
            Command::Rollback { .. } => "rollback",
            Command::Health { .. } => "health",
            Command::Check { .. } => "check",
            Command::Migrate { .. } => "migrate",
            Command::Repair => "repair",
            Command::ShowConfig => "show-config",
            Command::Setup { .. } => "setup",
            Command::Uninstall { .. } => "uninstall",
            Command::SelfUpdate { .. } => "self-update",
        }
    }
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Minimal ANSI styling, honouring `NO_COLOR` and a non-TTY stdout.
#[derive(Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn detect(disabled: bool) -> Self {
        let enabled = !disabled
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::IsTerminal::is_terminal(&std::io::stdout());
        Style { enabled }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, t: &str) -> String {
        self.paint("1", t)
    }
    pub fn dim(self, t: &str) -> String {
        self.paint("2", t)
    }
    pub fn green(self, t: &str) -> String {
        self.paint("32", t)
    }
    pub fn yellow(self, t: &str) -> String {
        self.paint("33", t)
    }
    pub fn red(self, t: &str) -> String {
        self.paint("31", t)
    }
    pub fn cyan(self, t: &str) -> String {
        self.paint("36", t)
    }

    pub fn urgency(self, u: Urgency, t: &str) -> String {
        match u {
            Urgency::Info => self.cyan(t),
            Urgency::Warning => self.yellow(t),
            Urgency::Critical => self.red(t),
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_status(s: &Status, style: Style) -> String {
    let mut out = String::new();

    for c in &s.components {
        let installed = c
            .eval
            .installed
            .as_ref()
            .map_or_else(|| "(not installed)".into(), Evr::to_string);

        // Known-good and lifecycle only mean something where sluice gates.
        let gates = c.eval.policy != crate::config::Policy::Follow || c.known_good.is_some();
        let state_tag = match c.lifecycle {
            _ if !gates => String::new(),
            Lifecycle::Unvetted => format!(" [{}]", style.dim("unvetted")),
            Lifecycle::KnownGood => format!(" [{}]", style.green("known-good")),
            Lifecycle::Testing => format!(" [{}]", style.yellow("testing")),
        };

        out.push_str(&format!(
            "{} — {}{}  policy: {}\n",
            style.bold(&c.eval.component),
            style.bold(&installed),
            state_tag,
            c.eval.policy.label()
        ));

        if let Some(series) = &c.eval.series {
            let mut line = format!("  series {series}");
            if let Some(u) = &c.upstream {
                line.push_str(&format!("   upstream: {}", u.describe()));
            }
            if let Some(days) = c.upstream_eol_days {
                line.push_str(&style.red(&format!(" (EOL {})", crate::app::eol_age(days))));
            }
            out.push_str(&line);
            out.push('\n');
        }

        let headline = match &c.eval.decision {
            Decision::Gated { .. } => style.yellow(&c.headline()),
            _ if c.eval.gated.is_some() => style.yellow(&c.headline()),
            Decision::Apply { .. } => style.cyan(&c.headline()),
            _ if c.eval.series_withdrawn => style.yellow(&c.headline()),
            _ => style.dim(&c.headline()),
        };
        out.push_str(&format!("  {headline}\n"));

        if let Some(kg) = &c.known_good {
            let vault = if c.known_good_vaulted {
                style.green("vaulted")
            } else {
                style.red("NOT vaulted — no rollback target once the repo drops it")
            };
            out.push_str(&format!("  known-good {kg} ({vault})\n"));
        } else if gates {
            out.push_str(&format!(
                "  {}\n",
                style.yellow(&format!(
                    "no known-good version recorded — run `sluice mark-good {}`",
                    c.eval.component
                ))
            ));
        }
        if let Some(held) = &c.eval.held_at {
            out.push_str(&format!(
                "  {}\n",
                style.yellow(&format!(
                    "held at {held} after a rollback — `sluice promote {}` releases it",
                    c.eval.component
                ))
            ));
        }
        if let Some(drift) = &c.boot_drift {
            out.push_str(&format!("  {}\n", style.yellow(drift)));
        }

        if let Some(h) = &c.health {
            out.push_str(&format!("  boots: {}\n", h.summary()));
        }
        out.push('\n');
    }

    if let Some(esp) = &s.esp {
        let text = format!(
            "ESP {}: {} MB free of {} MB ({}% used)",
            esp.path.display(),
            esp.free_mb,
            esp.total_mb,
            esp.used_percent()
        );
        out.push_str(&if s.esp_low {
            format!("{}\n", style.yellow(&text))
        } else {
            format!("{}\n", style.dim(&text))
        });
    }

    if s.vault_bytes > 0 {
        out.push_str(&style.dim(&format!(
            "vault: {:.1} MB\n",
            s.vault_bytes as f64 / 1_048_576.0
        )));
    }

    if let Some(entries) = &s.entries {
        let default = entries.iter().find(|e| e.is_default);
        if let Some(d) = default {
            out.push_str(&style.dim(&format!(
                "boot default: {}\n",
                d.version.as_deref().unwrap_or(&d.id)
            )));
        }
    }

    for w in &s.warnings {
        out.push_str(&format!("{}\n", style.dim(&format!("note: {w}"))));
    }
    match &s.last_update {
        Some(run) if !run.ok => {
            out.push_str(&format!(
                "{}\n",
                style.red(&format!(
                    "the last update ({}) failed: {}",
                    run.started
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M"),
                    run.error.as_deref().unwrap_or("unknown error")
                ))
            ));
        }
        Some(run) => out.push_str(&style.dim(&format!(
            "last update: {}{}\n",
            run.finished.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
            run.summary.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()
        ))),
        None => {}
    }
    if let Some(reason) = &s.reboot_needed {
        out.push_str(&format!(
            "{}\n",
            style.cyan(&format!("reboot to finish updating: {reason}"))
        ));
    }
    if let Some(r) = &s.new_release {
        out.push_str(&format!(
            "{}\n",
            style.cyan(&format!(
                "sluice {} is available (this is {}) — `sudo sluice self-update`",
                r.version(),
                crate::selfupdate::CURRENT
            ))
        ));
    }

    out
}

pub fn render_update(r: &UpdateReport, style: Style, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str(&style.dim("dry run — nothing was changed\n\n"));
    }

    for change in &r.lock_changes {
        out.push_str(&format!("{}\n", style.dim(change)));
    }
    if !r.lock_changes.is_empty() {
        out.push('\n');
    }

    if let Some(s) = &r.summary {
        out.push_str(&format!("{}\n", style.dim(&format!("zypper: {s}"))));
    }
    if r.applied.is_empty() {
        out.push_str(&style.dim("nothing to apply\n"));
    } else {
        for (component, version) in &r.applied {
            out.push_str(&format!(
                "{} {component} → {version}\n",
                style.green("updated")
            ));
        }
    }

    for (component, series, version) in &r.gated {
        out.push_str(&format!(
            "{} {component}: {series} is available ({version}) — run `sluice lineage {component}` to see it, \
             `sluice promote {component}` to take it\n",
            style.yellow("gated")
        ));
    }

    for (component, version, ready) in &r.soaking {
        out.push_str(&format!(
            "{} {component}: {version} settles on {}\n",
            style.dim("soaking"),
            ready.format("%Y-%m-%d")
        ));
    }

    for (component, version) in &r.held {
        out.push_str(&format!(
            "{} {component}: {version} is available\n",
            style.dim("held")
        ));
    }

    for note in &r.notes {
        out.push_str(&format!("{}\n", style.yellow(&format!("note: {note}"))));
    }
    if let Some(reason) = &r.reboot_needed {
        out.push_str(&format!(
            "{}\n",
            style.cyan(&format!("reboot to finish: {reason}"))
        ));
    }

    out
}

pub fn render_lineage(v: &LineageView, style: Style) -> String {
    let mut out = String::new();

    for series in [v.candidate.as_ref(), v.current.as_ref()]
        .into_iter()
        .flatten()
    {
        let label = if series.is_current {
            format!("{} {}", series.series, style.dim("(your series)"))
        } else if series.gated {
            format!("{} {}", series.series, style.yellow("(GATED)"))
        } else {
            format!("{} {}", series.series, style.dim("(requested)"))
        };

        let status = series.status.as_ref().map_or_else(
            || "upstream: unknown".into(),
            |s| format!("upstream: {}", s.describe()),
        );
        let mainline = series
            .status
            .as_ref()
            .and_then(|s| s.released)
            .map(|d| {
                let days = (Utc::now().date_naive() - d).num_days();
                format!("   mainline {d} ({days} days ago)")
            })
            .unwrap_or_default();
        let status_text = if series.status.as_ref().is_some_and(|s| s.eol) {
            style.red(&status)
        } else {
            style.dim(&status)
        };
        out.push_str(&format!(
            "{}{}   {}\n",
            style.bold(&label),
            style.dim(&mainline),
            status_text
        ));

        if series.points.is_empty() {
            out.push_str(&format!("  {}\n", style.dim("no point releases yet")));
        }
        if series.omitted > 0 {
            out.push_str(&format!(
                "  {}\n",
                style.dim(&format!(
                    "… {} earlier release(s) not shown",
                    series.omitted
                ))
            ));
        }

        for p in &series.points {
            let date = p
                .date
                .map_or_else(|| "          ".into(), |d| d.to_string());
            let mut line = format!("  {:<8} {date}   {:>4} patches", p.version, p.patches);
            for (label, count) in &p.highlights {
                line.push_str(&format!("   {label}: {count:>3}"));
            }
            if p.reverts > 0 {
                line.push_str(&format!("   reverts: {}", p.reverts));
            }
            out.push_str(&line);
            out.push('\n');
        }

        if let Some(hint) = &series.hint {
            out.push_str(&format!("  {}\n", style.cyan(&format!("hint: {hint}"))));
        }
        if let Some(note) = &series.repo_note {
            out.push_str(&format!("  {}\n", style.dim(note)));
        }
        if !series.repo_changelog.is_empty() {
            out.push_str(&format!("  {}\n", style.dim("distribution changelog:")));
            for entry in &series.repo_changelog {
                for line in entry.lines() {
                    out.push_str(&format!("    {}\n", style.dim(line)));
                }
            }
        }
        if series.withdrawn {
            out.push_str(&format!(
                "  {}\n",
                style.yellow("the repositories no longer ship this series — you are frozen here")
            ));
        }
        out.push('\n');
    }

    out.push_str(&style.dim(&format!("{}\n", v.freshness_note())));
    for w in &v.warnings {
        out.push_str(&style.dim(&format!("note: {w}\n")));
    }
    out
}

pub fn render_promote_preview(previews: &[crate::app::PromotePreview], style: Style) -> String {
    let mut out = String::new();
    for p in previews {
        let head = match (&p.from, &p.to) {
            (Some(f), Some(t)) => format!("{}  {f} → {t}", p.component),
            (None, Some(t)) => format!("{}  → {t}", p.component),
            _ => format!("{}  (stays)", p.component),
        };
        out.push_str(&format!("{}\n", style.bold(&head)));
        for b in &p.blockers {
            out.push_str(&format!("  {}\n", style.red(&format!("✖ {b}"))));
        }
        if p.to.is_none() {
            continue;
        }
        if !p.packages.is_empty() {
            out.push_str(&format!("  installs {}\n", p.packages.join(", ")));
        }
        if let Some(kg) = &p.known_good {
            out.push_str(&format!(
                "  {}\n",
                if p.known_good_vaulted {
                    style.green(&format!("✔ rollback target {kg}, vaulted"))
                } else {
                    style.yellow(&format!("rollback target {kg} is NOT vaulted"))
                }
            ));
        }
        let now = if p.gate_now.is_empty() {
            "no lock".to_string()
        } else {
            p.gate_now.join(", ")
        };
        let after = if p.gate_after.is_empty() {
            "no lock".to_string()
        } else {
            p.gate_after.join(", ")
        };
        out.push_str(&format!("  gate  {now}  →  {after}\n"));
        for n in &p.notes {
            out.push_str(&format!("  {}\n", style.dim(n)));
        }
    }
    out.push('\n');
    out
}

pub fn render_check(r: &CheckReport, style: Style) -> String {
    let mut out = String::new();
    if r.alerts.is_empty() {
        out.push_str(&style.dim("nothing needs attention\n"));
    }
    for (urgency, msg) in &r.alerts {
        out.push_str(&format!(
            "{}\n",
            style.urgency(*urgency, &format!("• {msg}"))
        ));
    }
    for p in &r.notify_problems {
        out.push_str(&style.yellow(&format!("notification not delivered — {p}\n")));
    }
    for n in &r.notes {
        out.push_str(&style.dim(&format!("note: {n}\n")));
    }
    out
}

pub fn render_migrate(r: &MigrateReport, style: Style) -> String {
    let mut out = String::new();
    let verb = if r.undone { "restored" } else { "migrated" };
    out.push_str(&format!("{}\n\n", style.bold(&format!("sluice {verb}"))));

    if !r.undone {
        out.push_str(&style.dim("locks found before migration:\n"));
        if r.prior_locks.is_empty() {
            out.push_str(&style.dim("  (none)\n"));
        }
        for l in &r.prior_locks {
            out.push_str(&style.dim(&format!("  {}\n", l.spec_string())));
        }
        out.push('\n');
    }

    for c in &r.lock_changes {
        out.push_str(&format!("{c}\n"));
    }
    for n in &r.notes {
        out.push_str(&format!("{}\n", style.dim(n)));
    }

    if !r.undone {
        out.push_str(&format!(
            "\n{}\n",
            style.dim("`sluice migrate --undo` restores exactly this state.")
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn run(cli: Cli) -> Result<i32> {
    let style = Style::detect(cli.no_color);
    // Setup and uninstall run before (and without) a configuration.
    match &cli.command {
        Some(Command::Setup { yes, apply }) => {
            return match apply {
                Some(plan) => crate::setup::apply(plan, style).map(|_| 0),
                None => crate::setup::run(*yes, style),
            };
        }
        Some(Command::Uninstall { purge }) => return crate::setup::uninstall(*purge, style),
        _ => {}
    }

    let mut config = Config::discover(cli.config.as_deref())?;
    if cli.offline {
        config.lineage.offline = true;
    }
    let now = Utc::now();

    let command = cli.command.unwrap_or(Command::Tui);

    // A dry run changes nothing, so it is the one way to try a mutating
    // command without root.
    if command.needs_root() && !cli.dry_run {
        privilege::require_root(command.name())?;
    }

    if matches!(command, Command::ShowConfig) {
        println!("{}", toml::to_string_pretty(&config)?);
        return Ok(0);
    }

    if matches!(command, Command::Tui) {
        return crate::tui::run(config, cli.dry_run);
    }

    let mut app = App::new(config, cli.dry_run)?;

    match command {
        Command::Tui | Command::ShowConfig | Command::Setup { .. } | Command::Uninstall { .. } => {
            unreachable!("handled above")
        }

        Command::Status => {
            let status = app.status(now)?;
            print!("{}", render_status(&status, style));
        }

        Command::Update => {
            let report = app.update(now)?;
            print!("{}", render_update(&report, style, cli.dry_run));
        }

        Command::Lineage { component, series } => {
            let view = app.lineage(&component, series.as_deref(), now)?;
            print!("{}", render_lineage(&view, style));
        }

        Command::Promote { component, yes } => {
            let bundle = app.config.bundles.contains_key(&component);
            let members: Vec<String> = if bundle {
                app.config.bundle(&component)?.components.clone()
            } else {
                vec![component.clone()]
            };

            // The lineage goes on screen before the question, so the decision is
            // made with the evidence in front of you rather than from memory.
            for m in &members {
                let has_lineage =
                    app.config.component(m)?.lineage != crate::config::LineageSource::None;
                if has_lineage {
                    if bundle {
                        println!("{}", style.bold(&format!("── {m}")));
                    }
                    let view = app.lineage(m, None, now)?;
                    print!("{}", render_lineage(&view, style));
                }
            }

            let previews = app.promote_preview(&members, now)?;
            print!("{}", render_promote_preview(&previews, style));
            let moving = previews.iter().any(|p| p.to.is_some());
            let fatal = previews
                .iter()
                .any(|p| !p.blockers.is_empty() && (p.to.is_some() || previews.len() == 1));
            if !moving || fatal {
                println!(
                    "{}",
                    style.red("cannot promote until the ✖ above is dealt with")
                );
                return Ok(1);
            }

            let what = if bundle {
                format!("Promote the {component} bundle ({})", members.join(", "))
            } else {
                format!("Promote {component}")
            };
            if !yes
                && !confirm(&format!(
                    "{what} across the series gate? This is a feature upgrade"
                ))?
            {
                println!("{}", style.dim("cancelled"));
                return Ok(1);
            }

            let reports = if bundle {
                app.promote_bundle(&component, now)?
            } else {
                vec![app.promote(&component, now)?]
            };
            for report in &reports {
                if report.packages.is_empty() {
                    println!("{} {} {}", style.dim("kept"), report.component, report.to);
                } else {
                    println!(
                        "{} {} {} → {} ({} packages)",
                        style.green("promoted"),
                        report.component,
                        report
                            .from
                            .as_ref()
                            .map_or_else(|| "?".into(), Evr::to_string),
                        report.to,
                        report.packages.len()
                    );
                }
                for n in &report.notes {
                    println!("  {}", style.dim(n));
                }
            }
            println!(
                "{}",
                style.dim(&format!(
                    "now testing. Reboot, use it, then `sluice mark-good {component}` — or `sluice rollback {component}`."
                ))
            );
        }

        Command::MarkGood { component, version } => {
            let reports = if app.config.bundles.contains_key(&component) {
                anyhow::ensure!(
                    version.is_none(),
                    "a bundle is marked good at its installed versions; name a component to pick a version"
                );
                app.mark_good_bundle(&component, now)?
            } else {
                let v = version.as_deref().map(Evr::parse);
                vec![app.mark_good(&component, v, now)?]
            };
            for report in reports {
                println!(
                    "{} {} {} as known-good",
                    style.green("marked"),
                    report.component,
                    report.version
                );
                if let Some(h) = &report.health {
                    println!("  {}", style.dim(&format!("boots: {}", h.summary())));
                }
                if !report.vaulted {
                    println!(
                        "  {}",
                        style.yellow("warning: its RPMs are not vaulted, so it cannot be reinstalled once the repository drops it")
                    );
                }
                for n in &report.notes {
                    println!("  {}", style.dim(n));
                }
            }
        }

        Command::Rollback { component, remove } => {
            let reports = if app.config.bundles.contains_key(&component) {
                app.rollback_bundle(&component, remove, now)?
            } else {
                vec![app.rollback(&component, remove, now)?]
            };
            for report in reports {
                println!(
                    "{} {} → {}",
                    style.green("rolled back"),
                    report.component,
                    report.to
                );
                for s in &report.steps {
                    println!("  {s}");
                }
                if let Some(n) = &report.note {
                    println!("{}", style.yellow(&format!("note: {n}")));
                }
            }
            println!("{}", style.dim("reboot to take effect."));
        }

        Command::Health { kernel } => {
            let report = app.health_report()?;
            if let Some(reason) = &report.unavailable {
                println!("{}", style.yellow(reason));
                return Ok(1);
            }
            if let Some(note) = &report.kernels_unknown {
                println!("{}", style.yellow(&format!("note: {note}")));
            }
            for h in report.by_kernel() {
                if kernel.as_deref().is_some_and(|k| k != h.kernel) {
                    continue;
                }
                let line = format!("{:<22} {}", h.kernel, h.summary());
                println!(
                    "{}",
                    if h.unclean_ends > 0 {
                        style.yellow(&line)
                    } else {
                        line
                    }
                );
            }
            for b in report.unclean() {
                println!(
                    "  {}",
                    style.dim(&format!(
                        "unclean end: {} ({})",
                        crate::health::local_window(b),
                        b.kernel.as_deref().unwrap_or("unknown kernel")
                    ))
                );
            }
        }

        Command::Check { force } => {
            let report = app.check(now, force)?;
            print!("{}", render_check(&report, style));
            // A non-zero exit lets a timer or a status bar react without parsing.
            return Ok(i32::from(!report.alerts.is_empty()));
        }

        Command::Migrate { known_good, undo } => {
            let report = if undo {
                app.migrate_undo(now)?
            } else {
                let choices = resolve_known_good(&mut app, &known_good, now)?;
                app.migrate(&choices, now)?
            };
            print!("{}", render_migrate(&report, style));
        }

        Command::SelfUpdate {
            check,
            rollback,
            yes,
            post_install,
        } => return self_update(&app, style, check, rollback, yes, post_install),

        Command::Repair => {
            let notes = app.repair(now)?;
            if notes.is_empty() {
                println!("{}", style.dim("nothing to repair"));
            }
            for n in notes {
                println!("{} {n}", style.green("repaired"));
            }
        }
    }

    Ok(0)
}

fn self_update(
    app: &App,
    style: Style,
    check: bool,
    rollback: bool,
    yes: bool,
    post_install: bool,
) -> Result<i32> {
    use crate::selfupdate::{self as su, CURRENT};
    let previous = su::previous_path(&app.config.paths.state_dir);

    if post_install {
        // The new binary brings its own systemd units; refresh any installed.
        let installed: Vec<&str> = crate::setup::UNITS
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| {
                std::path::Path::new(crate::setup::UNIT_DIR)
                    .join(n)
                    .exists()
            })
            .collect();
        for (name, text) in crate::setup::UNITS {
            if installed.contains(name) {
                std::fs::write(
                    std::path::Path::new(crate::setup::UNIT_DIR).join(name),
                    text,
                )?;
            }
        }
        if !installed.is_empty() {
            let _ = std::process::Command::new("systemctl")
                .arg("daemon-reload")
                .status();
            println!("  {} refreshed the systemd units", style.green("✔"));
        }
        return Ok(0);
    }

    if rollback {
        let target = su::target_binary()?;
        let version = su::rollback(&target, &previous)?;
        println!(
            "{} {version} is back at {}",
            style.green("rolled back:"),
            target.display()
        );
        return Ok(0);
    }

    let release = su::latest(
        &app.config.self_update,
        &app.config.lineage,
        &app.config.paths.cache_dir,
        true,
    )?;
    if !release.is_newer_than(CURRENT) {
        println!(
            "{}",
            style.dim(&format!("sluice {CURRENT} is the latest release"))
        );
        return Ok(0);
    }

    println!(
        "{}",
        style.bold(&format!("sluice {CURRENT} → {}", release.version()))
    );
    if let Some(when) = release.published_at.as_deref() {
        println!(
            "  {}",
            style.dim(&format!(
                "released {}",
                when.split('T').next().unwrap_or(when)
            ))
        );
    }
    for line in release.notes_excerpt(8) {
        println!("  {}", style.dim(&line));
    }
    if !release.html_url.is_empty() {
        println!("  {}", style.dim(&release.html_url));
    }
    if check {
        println!("`sudo sluice self-update` installs it.");
        return Ok(1);
    }

    let target = su::target_binary()?;
    println!();
    println!("This will download it, check it against the release's SHA256SUMS, run it");
    println!(
        "once to confirm it works, then replace {}.",
        target.display()
    );
    println!("The current version is kept: `sudo sluice self-update --rollback` puts it back.");
    if !yes && !confirm("Install it?")? {
        println!("{}", style.dim("nothing was changed"));
        return Ok(1);
    }

    let bytes = su::fetch_verified(&release)?;
    println!("  {} downloaded and verified", style.green("✔"));
    su::install(&bytes, release.version(), &target, &previous)?;
    println!(
        "  {} installed sluice {} at {}",
        style.green("✔"),
        release.version(),
        target.display()
    );
    let _ = std::process::Command::new(&target)
        .args(["self-update", "--post-install"])
        .status();
    Ok(0)
}

/// Turn `--known-good` arguments into a component → version map, asking on
/// the terminal for any component with more than one installed version that
/// was not named. With no terminal to ask on, the choice must be explicit.
fn resolve_known_good(
    app: &mut App,
    args: &[String],
    now: chrono::DateTime<Utc>,
) -> Result<std::collections::BTreeMap<String, Evr>> {
    let candidates = app.migrate_candidates(now)?;
    let mut out = std::collections::BTreeMap::new();

    for arg in args {
        let (component, version) = match arg.split_once('=') {
            Some((c, v)) => (c.to_string(), Evr::parse(v)),
            None => {
                let v = Evr::parse(arg);
                let owners: Vec<&String> = candidates
                    .iter()
                    .filter(|(_, installed)| installed.contains(&v))
                    .map(|(c, _)| c)
                    .collect();
                match owners.as_slice() {
                    [one] => ((*one).clone(), v),
                    [] => anyhow::bail!("no gated component has {arg} installed"),
                    _ => anyhow::bail!("{arg} is ambiguous; write it as COMPONENT={arg}"),
                }
            }
        };
        out.insert(component, version);
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    for (component, installed) in &candidates {
        if out.contains_key(component) || installed.len() < 2 {
            continue;
        }
        anyhow::ensure!(
            interactive,
            "{component} has several versions installed ({}); choose one with --known-good {component}=VERSION",
            installed.iter().map(Evr::to_string).collect::<Vec<_>>().join(", ")
        );
        println!("Which installed {component} version is known-good?");
        for (i, v) in installed.iter().enumerate() {
            println!("  {}) {v}", i + 1);
        }
        let newest = installed.len();
        print!("choice [{newest}]: ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let pick = match line.trim() {
            "" => newest,
            n => n
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=newest).contains(n))
                .with_context(|| format!("`{}` is not one of the choices", line.trim()))?,
        };
        out.insert(component.clone(), installed[pick - 1].clone());
    }
    Ok(out)
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_opens_the_dashboard() {
        let cli = Cli::try_parse_from(["sluice"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn mutating_commands_require_root_and_read_only_ones_do_not() {
        assert!(Command::Update.needs_root());
        assert!(Command::Promote {
            component: "kernel".into(),
            yes: false
        }
        .needs_root());
        assert!(Command::Repair.needs_root());

        assert!(!Command::Status.needs_root());
        assert!(!Command::Check { force: false }.needs_root());
        assert!(!Command::Health { kernel: None }.needs_root());
        assert!(!Command::Lineage {
            component: "kernel".into(),
            series: None
        }
        .needs_root());
    }

    #[test]
    fn global_flags_parse_after_the_subcommand() {
        let cli = Cli::try_parse_from(["sluice", "update", "--dry-run"]).unwrap();
        assert!(cli.dry_run);
        assert!(matches!(cli.command, Some(Command::Update)));
    }

    #[test]
    fn lineage_takes_an_optional_series() {
        let cli = Cli::try_parse_from(["sluice", "lineage", "kernel", "7.3"]).unwrap();
        let Some(Command::Lineage { component, series }) = cli.command else {
            panic!("expected lineage");
        };
        assert_eq!(component, "kernel");
        assert_eq!(series.as_deref(), Some("7.3"));
    }

    #[test]
    fn style_emits_nothing_when_disabled() {
        let plain = Style { enabled: false };
        assert_eq!(plain.red("x"), "x");
        let colored = Style { enabled: true };
        assert_eq!(colored.red("x"), "\x1b[31mx\x1b[0m");
    }
}
