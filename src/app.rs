//! Orchestration: the commands, as data-returning operations.
//!
//! Every command is a method here that returns a report rather than printing
//! one. The CLI renders those reports as text and the TUI renders them as
//! widgets, so the two front-ends can never drift apart in what they believe
//! about the system.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;

use crate::backend::{zypper::Zypper, LockSpec, PackageBackend, Pkg};
use crate::boot::{self, BootEntry, EspStatus};
use crate::config::{ComponentConfig, Config, Policy};
use crate::exec::Runner;
use crate::gate;
use crate::health::{self, HealthReport, KernelHealth};
use crate::lineage::{self, LineageView, PointRelease, SeriesLineage};
use crate::notify::{self, Notification, Urgency};
use crate::policy::{self, Decision, Evaluation};
use crate::state::{Lifecycle, MigrationRecord, State};
use crate::vault;
use crate::version::{series_regex, Evr};

pub struct App {
    pub config: Config,
    pub state: State,
    pub runner: Runner,
    pub backend: Box<dyn PackageBackend>,
    /// `uname -r` of the running kernel, so `rollback --remove` can refuse to
    /// uninstall the kernel the machine is running on.
    pub running_kernel: Option<String>,
}

/// Everything `status` and the TUI dashboard show.
pub struct Status {
    pub components: Vec<ComponentStatus>,
    pub esp: Option<EspStatus>,
    pub esp_low: bool,
    pub entries: Option<Vec<BootEntry>>,
    pub health: HealthReport,
    pub vault_bytes: u64,
    pub warnings: Vec<String>,
    /// A newer sluice release, if one is out.
    pub new_release: Option<crate::selfupdate::Release>,
    /// Why a reboot is due, if one is.
    pub reboot_needed: Option<String>,
    pub last_update: Option<crate::state::UpdateRun>,
}

pub struct ComponentStatus {
    pub eval: Evaluation,
    pub lifecycle: Lifecycle,
    pub known_good: Option<Evr>,
    pub known_good_vaulted: bool,
    pub health: Option<KernelHealth>,
    /// Upstream's view of the installed series, when a lineage source exists.
    pub upstream: Option<lineage::SeriesStatus>,
    pub upstream_eol_days: Option<i64>,
    /// Set when the default boot entry is not on the version this component
    /// is holding — typically because a zypper transaction's snapshot hook
    /// reset it to the newest installed kernel.
    pub boot_drift: Option<String>,
}

impl ComponentStatus {
    /// The single line that says what needs attention, if anything.
    pub fn headline(&self) -> String {
        Self::headline_of(&self.eval)
    }

    pub fn headline_of(eval: &Evaluation) -> String {
        match &eval.decision {
            Decision::UpToDate if eval.series_withdrawn => {
                "up to date, but this series is no longer shipped".into()
            }
            Decision::UpToDate => "up to date".into(),
            Decision::Apply { target } => match eval.gated_series() {
                Some((series, _)) => format!("will update to {target}; {series} is gated"),
                None => format!("will update to {target}"),
            },
            Decision::Gated { series, target } => {
                format!("GATED: {series} available ({target})")
            }
            Decision::Soaking { target, ready_at } => {
                let days = (*ready_at - Utc::now()).num_days().max(0);
                format!("soaking {target} ({days} day(s) to go)")
            }
            Decision::Held { target } => format!("held: {target} available"),
        }
    }

    /// Reasons this component should be escalated to the user, worst first.
    pub fn alerts(&self, cfg: &ComponentConfig) -> Vec<(Urgency, String)> {
        let mut out = Vec::new();

        if let Some((series, target)) = self.eval.gated_series() {
            out.push((
                Urgency::Info,
                format!("a new {series} series is available ({target}), held for your decision"),
            ));
        }

        if self.eval.series_withdrawn {
            out.push((
                Urgency::Warning,
                format!(
                    "the repositories no longer ship {}; you are frozen at {} and will receive no further fixes",
                    self.eval.series.as_deref().unwrap_or("this series"),
                    self.eval.installed.as_ref().map_or_else(|| "?".into(), Evr::to_string),
                ),
            ));
        }

        if let Some(days) = self.upstream_eol_days {
            let urgency = if days >= i64::from(cfg.warn_after_eol_days) {
                Urgency::Critical
            } else {
                Urgency::Warning
            };
            out.push((
                urgency,
                format!(
                    "{} has been end-of-life upstream for {days} day(s); it is receiving no security fixes",
                    self.eval.series.as_deref().unwrap_or("this series")
                ),
            ));
        }

        if let Some(drift) = &self.boot_drift {
            out.push((Urgency::Warning, drift.clone()));
        }

        out.sort_by_key(|a| std::cmp::Reverse(a.0));
        out
    }
}

impl App {
    pub fn new(config: Config, dry_run: bool) -> Result<Self> {
        let state = State::load(&config.paths.state_file())?;
        let runner = Runner::new(dry_run, Some(config.paths.log_file.clone()));
        let backend: Box<dyn PackageBackend> = Box::new(Zypper::default());
        let running_kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string());
        Ok(App {
            config,
            state,
            runner,
            backend,
            running_kernel,
        })
    }

    pub fn with_backend(mut self, backend: Box<dyn PackageBackend>) -> Self {
        self.backend = backend;
        self
    }

    pub fn save(&self) -> Result<()> {
        if self.runner.dry_run() {
            return Ok(());
        }
        self.state.save(&self.config.paths.state_file())
    }

    /// Restore any locks left off by an interrupted run. Called before anything
    /// that touches packages.
    pub fn repair_locks(&mut self) -> Result<Option<String>> {
        let recovered = gate::recover(
            self.backend.as_ref(),
            &mut self.runner,
            &self.config.paths.state_dir,
        )?;
        Ok(recovered.map(|p| {
            format!(
                "restored {} lock(s) left off by an interrupted `{}` at {}",
                p.locks.len(),
                p.reason,
                p.opened_at.format("%Y-%m-%d %H:%M UTC")
            )
        }))
    }

    /// Put everything sluice guarantees back in place: locks left off by an
    /// interrupted run, and a default boot entry something else moved.
    pub fn repair(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let mut notes: Vec<String> = self.repair_locks()?.into_iter().collect();
        notes.extend(self.enforce_boot_defaults(now)?);
        Ok(notes)
    }

    // -----------------------------------------------------------------------
    // Package queries
    // -----------------------------------------------------------------------

    /// Installed and available packages for one component.
    fn packages(&mut self, cfg: &ComponentConfig) -> Result<(Vec<Pkg>, Vec<Pkg>)> {
        let mut patterns = cfg.packages.patterns();
        if cfg.family_sources.is_empty() {
            if let Some(g) = Config::family_glob_for(cfg) {
                patterns.push(g);
            }
        }
        if let Some(a) = &cfg.anchor {
            patterns.push(a.clone());
        }

        // A source-grouped family is found through what is installed from
        // those sources, since its members need not share a name prefix.
        let sources = if cfg.family_sources.is_empty() {
            Vec::new()
        } else {
            let all = self.backend.installed_details(&mut self.runner, &[])?;
            all.into_iter()
                .filter_map(|p| {
                    let src = p.source?;
                    cfg.family_sources
                        .contains(&src)
                        .then_some((p.name, p.evr, src))
                })
                .collect::<Vec<_>>()
        };
        patterns.extend(sources.iter().map(|(name, _, _)| name.clone()));
        patterns.sort();
        patterns.dedup();

        let mut all = self.backend.query_many(&mut self.runner, &patterns)?;
        for p in all.iter_mut().filter(|p| p.installed()) {
            if let Some((_, _, src)) = sources.iter().find(|(n, e, _)| n == &p.name && e == &p.evr)
            {
                p.source = Some(src.clone());
            }
        }
        let installed: Vec<Pkg> = all.iter().filter(|p| p.installed()).cloned().collect();
        Ok((installed, all))
    }

    pub fn evaluate(&mut self, now: DateTime<Utc>) -> Result<Vec<Evaluation>> {
        let components: Vec<(String, ComponentConfig)> = self
            .config
            .components
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut out = Vec::new();
        for (name, cfg) in components {
            let (installed, available) = self.packages(&cfg)?;
            let state = self.state.component_mut(&name);
            out.push(policy::evaluate(
                &name, &cfg, state, &installed, &available, now,
            )?);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------

    pub fn status(&mut self, now: DateTime<Utc>) -> Result<Status> {
        let mut warnings = Vec::new();
        let evals = self.evaluate(now)?;

        let health = self.health_report()?;
        if let Some(reason) = health
            .unavailable
            .as_ref()
            .or(health.kernels_unknown.as_ref())
        {
            warnings.push(reason.clone());
        }

        let esp = boot::esp_status(&self.config.boot, &mut self.runner)?;
        let esp_low = esp
            .as_ref()
            .is_some_and(|e| e.is_low(self.config.boot.esp_warn_free_mb));
        let entries = boot::entries(&self.config.boot, &mut self.runner).unwrap_or(None);

        // Upstream status is best-effort: a lineage lookup that fails must not
        // stop `status` from telling you what is installed.
        let mut upstream_by_component: BTreeMap<String, lineage::SeriesStatus> = BTreeMap::new();
        let wants_upstream: Vec<&Evaluation> = evals
            .iter()
            .filter(|e| {
                self.config
                    .components
                    .get(&e.component)
                    .is_some_and(|c| c.lineage == crate::config::LineageSource::LinuxStable)
            })
            .collect();
        if !wants_upstream.is_empty() {
            let mut f = lineage::Fetcher::new(&self.config.lineage, &self.config.paths.cache_dir);
            if let Some(releases) = f.releases() {
                let mut indexes: BTreeMap<String, Vec<lineage::IndexEntry>> = BTreeMap::new();
                for eval in wants_upstream {
                    let Some(series) = &eval.series else { continue };
                    let re = series_regex(&self.config.component(&eval.component)?.series_regex)?;
                    let major = series.split('.').next().unwrap_or(series).to_string();
                    if !indexes.contains_key(&major) {
                        let idx = f.index(&major).unwrap_or_default();
                        indexes.insert(major.clone(), idx);
                    }
                    if let Some(st) =
                        lineage::series_status(&releases, &indexes[&major], series, &re)
                    {
                        upstream_by_component.insert(eval.component.clone(), st);
                    }
                }
            }
            warnings.extend(f.warnings);
        }

        let mut components = Vec::new();
        for eval in evals {
            let cstate = self.state.component(&eval.component);
            let upstream = upstream_by_component.remove(&eval.component);
            let upstream_eol_days = upstream
                .as_ref()
                .filter(|u| u.eol)
                .and_then(|u| u.eol_since)
                .map(|d| (now.date_naive() - d).num_days().max(0));

            let kernel_health = eval
                .installed
                .as_ref()
                .and_then(|v| match_kernel_health(&health, v));
            let boot_drift = self.boot_drift(&eval);

            components.push(ComponentStatus {
                boot_drift,
                known_good_vaulted: cstate
                    .known_good
                    .as_ref()
                    .is_some_and(|v| cstate.is_vaulted(v)),
                lifecycle: cstate.lifecycle,
                known_good: cstate.known_good.clone(),
                health: kernel_health,
                upstream,
                upstream_eol_days,
                eval,
            });
        }

        let reboot_needed = self.reboot_needed(&components);
        Ok(Status {
            components,
            esp,
            esp_low,
            entries,
            health,
            vault_bytes: vault::size_bytes(&self.config.paths.vault_dir),
            warnings,
            reboot_needed,
            last_update: self.state.last_update_run().cloned(),
            new_release: crate::selfupdate::available(
                &self.config.self_update,
                &self.config.lineage,
                &self.config.paths.cache_dir,
            ),
        })
    }

    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    /// Refresh, apply what the policies allow, and report. Every run is
    /// recorded — a failed unattended update must not pass unnoticed.
    pub fn update(&mut self, now: DateTime<Utc>) -> Result<UpdateReport> {
        let started = Utc::now();
        let result = self.update_inner(now);
        if !self.runner.dry_run() {
            let run = match &result {
                Ok(r) => crate::state::UpdateRun {
                    started,
                    finished: Utc::now(),
                    ok: true,
                    error: None,
                    applied: r.applied.iter().map(|(c, v)| format!("{c} {v}")).collect(),
                    summary: r.summary.clone(),
                },
                Err(e) => crate::state::UpdateRun {
                    started,
                    finished: Utc::now(),
                    ok: false,
                    error: Some(format!("{e:#}")),
                    applied: Vec::new(),
                    summary: None,
                },
            };
            self.state.record_update(run);
            if let Err(e) = self.save() {
                self.runner
                    .note(&format!("could not record the update run: {e:#}"));
            }
        }
        result
    }

    fn update_inner(&mut self, now: DateTime<Utc>) -> Result<UpdateReport> {
        let mut report = UpdateReport::default();
        if let Some(msg) = self.repair_locks()? {
            report.notes.push(msg);
        }

        if self.config.backend.refresh {
            match self.backend.refresh(&mut self.runner) {
                Ok(()) => {}
                // An unprivileged dry run cannot refresh metadata; it evaluates
                // against what is cached and says so.
                Err(e) if self.runner.dry_run() && !crate::privilege::is_root() => report
                    .notes
                    .push(format!("metadata not refreshed (needs root): {e:#}")),
                Err(e) => return Err(e),
            }
        }

        // Locks are reconciled before the upgrade, so `zypper dup` itself runs
        // with every gate already in place. Nothing is ever unlocked to run it.
        report.lock_changes = self.reconcile_locks(now)?;

        let evals = self.evaluate(now)?;
        let mut dup_args = Vec::new();
        if self.config.backend.auto_agree_licenses {
            dup_args.push("--auto-agree-with-licenses".to_string());
        }
        let transaction = self.backend.dist_upgrade(&mut self.runner, &dup_args)?;
        report.summary = transaction.summary;
        report.dup_ran = true;

        for eval in &evals {
            if let Some((series, target)) = eval.gated_series() {
                report
                    .gated
                    .push((eval.component.clone(), series.to_string(), target.clone()));
                self.state
                    .component_mut(&eval.component)
                    .record_gated(series, target, now);
            } else {
                self.state.component_mut(&eval.component).gated = None;
            }
            match &eval.decision {
                Decision::Apply { target } if !eval.targets.is_empty() => {
                    // The whole family moves in one transaction: a half-moved
                    // kernel family will not boot.
                    self.backend
                        .install_exact(&mut self.runner, &eval.targets)?;
                    report
                        .applied
                        .push((eval.component.clone(), target.clone()));

                    // A fix inside the series has landed, so a boot pin left by
                    // an earlier rollback has done its job.
                    let cstate = self.state.component_mut(&eval.component);
                    if cstate.boot_pin.as_ref().is_some_and(|p| target > p) {
                        cstate.boot_pin = None;
                    }

                    // Vaulted now, while the repository still has it, so it can
                    // become a rollback target once you mark it good.
                    let cfg = self.config.component(&eval.component)?.clone();
                    if let Some(note) = self.vault_version(&eval.component, &cfg, eval, target)? {
                        report.notes.push(note);
                    }
                }
                Decision::Soaking { target, ready_at } => {
                    report
                        .soaking
                        .push((eval.component.clone(), target.clone(), *ready_at));
                }
                Decision::Held { target } => {
                    report.held.push((eval.component.clone(), target.clone()));
                }
                _ => {}
            }
        }

        report.notes.extend(self.prune_vaults()?);

        // Checked after the fact: an update that installed cleanly but left the
        // known-good kernel unprotected, or the boot default somewhere else, is
        // still a problem worth naming.
        report.notes.extend(self.verify_retention()?);
        report.notes.extend(self.enforce_boot_defaults(now)?);
        // A run with root is the chance to record install history and which
        // kernel each boot ran, for the unprivileged views to use later.
        if let Err(e) = self.health_report() {
            report
                .notes
                .push(format!("boot evidence not recorded: {e:#}"));
        }
        self.state.last_update = Some(now);
        let evals = self.evaluate(now)?;
        let statuses: Vec<ComponentStatus> = evals
            .into_iter()
            .map(|eval| ComponentStatus {
                lifecycle: Lifecycle::Unvetted,
                known_good: None,
                known_good_vaulted: false,
                health: None,
                upstream: None,
                upstream_eol_days: None,
                boot_drift: None,
                eval,
            })
            .collect();
        report.reboot_needed = self.reboot_needed(&statuses);
        self.save()?;
        Ok(report)
    }

    /// Drop vaulted versions that are neither known-good, under test, nor
    /// still installed. The vault holds rollback targets, not history.
    fn prune_vaults(&mut self) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        let names: Vec<String> = self.config.components.keys().cloned().collect();
        for name in names {
            if self.state.component(&name).vault.is_empty() {
                continue;
            }
            let cfg = self.config.component(&name)?.clone();
            let (installed, _) = self.packages(&cfg)?;
            let keep: Vec<Evr> = installed.iter().map(|p| p.evr.clone()).collect();
            let vault_dir = self.config.paths.vault_dir.clone();
            let dry_run = self.runner.dry_run();
            let cstate = self.state.component_mut(&name);
            let pruned = vault::prune(&vault_dir, &name, cstate, &keep, dry_run)?;
            if !pruned.is_empty() {
                notes.push(format!(
                    "pruned {name} {} from the vault",
                    pruned
                        .iter()
                        .map(Evr::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        Ok(notes)
    }

    /// Bring locks in line with the policies. Returns a human-readable diff.
    ///
    /// Version-conditional locks mean a held series needs no unlock window:
    /// locking `>= <next series>` leaves every fix release inside the current
    /// series free to install.
    pub fn reconcile_locks(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let evals = self.evaluate(now)?;
        let existing = self.backend.locks(&mut self.runner)?;
        let mut changes = Vec::new();

        for eval in &evals {
            let cfg = self.config.component(&eval.component)?;
            let wanted = desired_locks(&eval.component, cfg, eval);

            // Only locks this component owns are considered, so a lock the user
            // added by hand for something else is never touched.
            let owned: Vec<&LockSpec> = existing.iter().filter(|l| owns(l, eval)).collect();

            for spec in &wanted {
                if !existing.contains(spec) {
                    self.backend.add_lock(&mut self.runner, spec)?;
                    changes.push(format!("+ lock {}", spec.spec_string()));
                }
            }
            for spec in owned {
                if !wanted.contains(spec) {
                    self.backend.remove_lock(&mut self.runner, spec)?;
                    changes.push(format!("- lock {}", spec.spec_string()));
                }
            }
        }
        Ok(changes)
    }

    /// Keep every known-good kernel pinned in `multiversion.kernels`, so
    /// purge-kernels cannot remove it, and drop pins sluice added for versions
    /// that are no longer known-good. Pins you added by hand are never touched.
    fn verify_retention(&mut self) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        let wanted: Vec<Evr> = self
            .config
            .components
            .iter()
            .filter(|(_, c)| c.boot_entries)
            .filter_map(|(name, _)| self.state.component(name).known_good)
            .collect();
        if wanted.is_empty() && self.state.pinned.is_empty() {
            return Ok(notes);
        }

        let zypp_conf = self.config.boot.package_manager_conf.clone();
        let mut mv = match boot::read_multiversion(&zypp_conf) {
            Ok(Some(mv)) => mv,
            Ok(None) => {
                notes.push(
                    "no multiversion.kernels setting found; the known-good kernel is not protected from purge-kernels".into(),
                );
                return Ok(notes);
            }
            Err(e) => {
                notes.push(format!("could not read multiversion.kernels: {e:#}"));
                return Ok(notes);
            }
        };

        let before = mv.clone();
        let stale: Vec<Evr> = self
            .state
            .pinned
            .iter()
            .filter(|p| !wanted.contains(p))
            .cloned()
            .collect();
        for p in &stale {
            mv.unpin(p);
        }
        let newly: Vec<Evr> = wanted
            .iter()
            .filter(|w| !before.contains(&w.to_string()))
            .cloned()
            .collect();
        mv.ensure(&self.config.boot.multiversion_base, &wanted);

        if mv != before {
            match boot::write_multiversion(&zypp_conf, &mv, self.runner.dry_run()) {
                Ok(_) => notes.push(format!("multiversion.kernels = {}", mv.render())),
                Err(e) => {
                    notes.push(format!(
                        "could not pin the known-good kernel against purge-kernels: {e:#}"
                    ));
                    return Ok(notes);
                }
            }
        }
        self.state.pinned.retain(|p| wanted.contains(p));
        self.state.pinned.extend(newly);
        Ok(notes)
    }

    // -----------------------------------------------------------------------
    // promote / mark-good / rollback
    // -----------------------------------------------------------------------

    /// Promote a component across the series gate — or out of a rollback hold.
    /// This is the only path that crosses a feature boundary, and it never
    /// runs without being asked.
    pub fn promote(&mut self, component: &str, now: DateTime<Utc>) -> Result<PromoteReport> {
        let mut reports = self.promote_many(&[component.to_string()], true, now)?;
        Ok(reports.remove(0))
    }

    /// Promote a bundle: every member that is gated (or held after a rollback)
    /// crosses in one transaction, and every member becomes `testing`, so the
    /// combination is what gets marked good or rolled back.
    pub fn promote_bundle(
        &mut self,
        bundle: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<PromoteReport>> {
        let members = self.config.bundle(bundle)?.components.clone();
        self.promote_many(&members, false, now)
    }

    fn promote_many(
        &mut self,
        members: &[String],
        single: bool,
        now: DateTime<Utc>,
    ) -> Result<Vec<PromoteReport>> {
        let mut notes: Vec<String> = self.repair_locks()?.into_iter().collect();

        struct Move {
            component: String,
            cfg: ComponentConfig,
            eval: Evaluation,
            target: Evr,
            series: Option<String>,
            packages: Vec<Pkg>,
        }
        let mut moves: Vec<Move> = Vec::new();
        let mut unchanged: Vec<(String, Evaluation)> = Vec::new();

        for component in members {
            let (cfg, eval) = self.eval_of(component, now)?;
            let (target, series) = match (eval.gated.clone(), eval.decision.clone()) {
                (Some((series, target)), _) => (target, Some(series)),
                (None, Decision::Held { target }) if eval.held_at.is_some() => (target, None),
                (None, d) => {
                    anyhow::ensure!(
                        !single,
                        "`{component}` has nothing gated to promote (currently: {})",
                        match d {
                            Decision::UpToDate => "up to date".to_string(),
                            d => format!("{d:?}"),
                        }
                    );
                    unchanged.push((component.clone(), eval));
                    continue;
                }
            };

            // Before crossing, make sure there is something to come back to.
            let cstate = self.state.component(component);
            let Some(known_good) = cstate.known_good.clone() else {
                anyhow::bail!(
                    "`{component}` has no known-good version recorded. \
                     Run `sluice mark-good {component}` first so there is a rollback target."
                );
            };
            if !cstate.is_vaulted(&known_good) {
                anyhow::bail!(
                    "the known-good version of `{component}` ({known_good}) is not vaulted, so a rollback \
                     could not be installed once the repository drops it. Run `sluice mark-good {component}` \
                     while the repository still offers it."
                );
            }

            let packages = self.family_at_target(&cfg, &eval, &target)?;
            anyhow::ensure!(
                !packages.is_empty(),
                "no packages found for {component} {target}"
            );
            moves.push(Move {
                component: component.clone(),
                cfg,
                eval,
                target,
                series,
                packages,
            });
        }
        anyhow::ensure!(
            !moves.is_empty(),
            "nothing in it is gated or held; there is nothing to promote"
        );

        // The gates come off only for this one transaction, and go back on after.
        let owned: Vec<LockSpec> = self
            .backend
            .locks(&mut self.runner)?
            .into_iter()
            .filter(|l| moves.iter().any(|m| owns(l, &m.eval)))
            .collect();
        let all: Vec<Pkg> = moves.iter().flat_map(|m| m.packages.clone()).collect();
        let state_dir = self.config.paths.state_dir.clone();
        {
            let window = gate::LockWindow::open(
                self.backend.as_ref(),
                &mut self.runner,
                &state_dir,
                owned,
                "promote",
            )?;
            window.check_interrupted()?;
            let result = self.backend.install_exact(&mut self.runner, &all);
            // The window is closed explicitly so a failure to re-lock is
            // reported rather than swallowed by Drop.
            window.close(&mut self.runner)?;
            result?;
        }

        let mut reports = Vec::new();
        for m in &moves {
            let cstate = self.state.component_mut(&m.component);
            cstate.lifecycle = Lifecycle::Testing;
            cstate.testing = Some(m.target.clone());
            if let Some(s) = &m.series {
                cstate.series = Some(s.clone());
            }
            cstate.held_at = None;
            cstate.boot_pin = None;
            cstate.gated = None;

            let eval_after = self.eval_of(&m.component, now)?.1;
            let mut member_notes = Vec::new();
            member_notes.extend(self.vault_version(
                &m.component,
                &m.cfg,
                &eval_after,
                &m.target,
            )?);
            reports.push(PromoteReport {
                component: m.component.clone(),
                from: m.eval.installed.clone(),
                to: m.target.clone(),
                series: m.series.clone().unwrap_or_default(),
                packages: m.packages.iter().map(Pkg::nevra).collect(),
                notes: member_notes,
            });
        }

        // Members that did not move are still part of what is being tested.
        for (component, eval) in unchanged {
            if let Some(v) = eval.installed {
                let cstate = self.state.component_mut(&component);
                cstate.lifecycle = Lifecycle::Testing;
                cstate.testing = Some(v.clone());
                reports.push(PromoteReport {
                    component,
                    from: Some(v.clone()),
                    to: v,
                    series: String::new(),
                    packages: Vec::new(),
                    notes: vec!["unchanged; recorded as part of what is under test".into()],
                });
            }
        }

        // New series need new gates: hold 7.4 now, not 7.3.
        self.reconcile_locks(now)?;
        notes.extend(self.enforce_boot_defaults(now)?);
        self.save()?;
        if let Some(first) = reports.first_mut() {
            first.notes.extend(notes);
        }
        Ok(reports)
    }

    /// What promoting would do, without doing any of it: the packages, the
    /// rollback target, how the gate moves, and anything that stands in the
    /// way. One entry per component (a bundle has several).
    pub fn promote_preview(
        &mut self,
        members: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<PromotePreview>> {
        let locks = self.backend.locks(&mut self.runner).unwrap_or_default();
        let mut out = Vec::new();
        for component in members {
            let (cfg, eval) = self.eval_of(component, now)?;
            let cstate = self.state.component(component);
            let mut p = PromotePreview {
                component: component.clone(),
                from: eval.installed.clone(),
                known_good: cstate.known_good.clone(),
                known_good_vaulted: cstate
                    .known_good
                    .as_ref()
                    .is_some_and(|v| cstate.is_vaulted(v)),
                gate_now: locks
                    .iter()
                    .filter(|l| owns(l, &eval))
                    .map(LockSpec::spec_string)
                    .collect(),
                ..Default::default()
            };

            let target = match (eval.gated.clone(), &eval.decision) {
                (Some((series, target)), _) => {
                    p.series = Some(series);
                    Some(target)
                }
                (None, Decision::Held { target }) if eval.held_at.is_some() => Some(target.clone()),
                _ => None,
            };
            let Some(target) = target else {
                p.blockers.push(format!(
                    "nothing is gated for {component}: {}",
                    ComponentStatus::headline_of(&eval)
                ));
                out.push(p);
                continue;
            };
            p.to = Some(target.clone());

            match &p.known_good {
                None => p.blockers.push(format!(
                    "no known-good version of {component} is recorded, so there would be nothing to roll back to — mark one good first"
                )),
                Some(kg) if !p.known_good_vaulted => p.blockers.push(format!(
                    "the known-good {kg} is not vaulted, so it could not be reinstalled once the repository drops it"
                )),
                _ => {}
            }

            let packages = self.family_at_target(&cfg, &eval, &target)?;
            if packages.is_empty() {
                p.blockers.push(format!(
                    "the repositories offer no packages for {component} {target}"
                ));
            }
            p.packages = packages.iter().map(Pkg::nevra).collect();

            // The gate after: held at the series after the new one, or — out
            // of a rollback hold — whatever the policy normally asks for.
            let names: Vec<String> = {
                let mut n: Vec<String> = eval
                    .family
                    .iter()
                    .filter(|x| !policy::is_kmp(&x.name))
                    .map(|x| x.name.clone())
                    .collect();
                n.extend(cfg.anchor.clone());
                n.sort();
                n.dedup();
                n
            };
            p.gate_after = match p.series.as_deref().and_then(gate::next_series) {
                Some(next) if cfg.policy == Policy::HoldSeries => names
                    .iter()
                    .map(|n| gate::series_lock(n, &next, component).spec_string())
                    .collect(),
                _ => Vec::new(),
            };

            if cfg.boot_entries {
                p.notes.push(format!(
                    "{target} becomes the default boot entry; the previous kernels stay installed"
                ));
            }
            if let Some(kg) = &p.known_good {
                p.notes.push(format!(
                    "if it misbehaves: `sluice rollback {component}` boots {kg} again and puts {} back behind the gate",
                    target
                ));
            }
            out.push(p);
        }
        Ok(out)
    }

    /// Mark every member of a bundle good at its installed version.
    pub fn mark_good_bundle(
        &mut self,
        bundle: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<MarkGoodReport>> {
        let members = self.config.bundle(bundle)?.components.clone();
        let mut out = Vec::new();
        for m in members {
            if self.eval_of(&m, now)?.1.installed.is_some() {
                out.push(self.mark_good(&m, None, now)?);
            }
        }
        Ok(out)
    }

    /// Roll every member of a bundle back to its known-good version.
    pub fn rollback_bundle(
        &mut self,
        bundle: &str,
        remove: bool,
        now: DateTime<Utc>,
    ) -> Result<Vec<RollbackReport>> {
        let members = self.config.bundle(bundle)?.components.clone();
        let missing: Vec<&String> = members
            .iter()
            .filter(|m| self.state.component(m).known_good.is_none())
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "no known-good version for {}; mark the bundle good before relying on rolling it back",
            missing
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut out = Vec::new();
        for m in members {
            out.push(self.rollback(&m, remove, now)?);
        }
        Ok(out)
    }

    /// The exact packages that make up `cfg`'s family at `target`, as the
    /// repositories offer them.
    ///
    /// If that version is installed, the installed family is used, including
    /// any KMPs built for it. If not, the names of the current family are
    /// looked up at `target`, and KMPs are matched to it by their build marker.
    fn family_at_target(
        &mut self,
        cfg: &ComponentConfig,
        eval: &Evaluation,
        target: &Evr,
    ) -> Result<Vec<Pkg>> {
        let (installed, available) = self.packages(cfg)?;
        let offered = |name: &str, evr: &Evr| {
            available
                .iter()
                .find(|p| p.name == name && &p.evr == evr && p.offered())
                .cloned()
        };

        let mut out: Vec<Pkg> = Vec::new();
        let at_installed: Vec<Pkg> = policy::resolve_family(cfg, &installed, Some(target))
            .into_iter()
            .filter(|p| policy::same_build(cfg, &p.evr, target) || policy::is_kmp(&p.name))
            .collect();

        if at_installed.iter().any(|p| &p.evr == target) {
            for p in at_installed {
                out.push(offered(&p.name, &p.evr).unwrap_or(p));
            }
        } else {
            let mut names: Vec<&str> = eval
                .family
                .iter()
                .filter(|p| !policy::is_kmp(&p.name))
                .map(|p| p.name.as_str())
                .collect();
            if let Some(a) = &cfg.anchor {
                names.push(a);
            }
            names.sort_unstable();
            names.dedup();
            for n in names {
                let exact = offered(n, target);
                let same_version = || {
                    available
                        .iter()
                        .filter(|p| {
                            p.name == n && p.offered() && policy::same_build(cfg, &p.evr, target)
                        })
                        .max_by(|a, b| a.evr.cmp(&b.evr))
                        .cloned()
                };
                if let Some(p) = exact.or_else(same_version) {
                    out.push(p);
                }
            }
            for kmp in eval.family.iter().filter(|p| policy::is_kmp(&p.name)) {
                if let Some(p) = available
                    .iter()
                    .filter(|p| p.name == kmp.name && policy::kmp_built_for(&p.evr, target))
                    .max_by(|a, b| a.evr.cmp(&b.evr))
                {
                    out.push(p.clone());
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name && a.evr == b.evr);
        Ok(out)
    }

    fn eval_of(
        &mut self,
        component: &str,
        now: DateTime<Utc>,
    ) -> Result<(ComponentConfig, Evaluation)> {
        let cfg = self.config.component(component)?.clone();
        let eval = self
            .evaluate(now)?
            .into_iter()
            .find(|e| e.component == component)
            .with_context(|| format!("component `{component}` was not evaluated"))?;
        Ok((cfg, eval))
    }

    /// Vault `version`'s RPMs unless they already are. Returns a note on failure.
    fn vault_version(
        &mut self,
        component: &str,
        cfg: &ComponentConfig,
        eval: &Evaluation,
        version: &Evr,
    ) -> Result<Option<String>> {
        let cstate = self.state.component(component);
        let already = cstate.vault.iter().any(|v| {
            &v.version == version && !v.files.is_empty() && v.files.iter().all(|f| f.exists())
        });
        if already {
            return Ok(None);
        }

        let targets = self.family_at_target(cfg, eval, version)?;
        let offered: Vec<Pkg> = targets.iter().filter(|p| p.offered()).cloned().collect();
        if offered.is_empty() {
            return Ok(Some(format!(
                "no RPMs for {component} {version} are obtainable from the configured repositories, \
                 so it could not be vaulted. Once it is uninstalled it cannot serve as a rollback target."
            )));
        }

        if self.runner.dry_run() {
            return Ok(Some(format!(
                "would vault {component} {version} ({} package(s))",
                offered.len()
            )));
        }
        self.seed_vault_from_cache(component, version);
        let vault_dir = self.config.paths.vault_dir.clone();
        let mut cstate = self.state.component(component);
        let result = vault::store(
            self.backend.as_ref(),
            &mut self.runner,
            &vault_dir,
            component,
            version,
            &offered,
            &mut cstate,
        );
        self.state.components.insert(component.to_string(), cstate);

        let unobtainable: Vec<String> = targets
            .iter()
            .filter(|p| !p.offered())
            .map(Pkg::nevra)
            .collect();
        Ok(match result {
            Err(e) => Some(format!("could not vault {component} {version}: {e:#}")),
            Ok(_) if !unobtainable.is_empty() => Some(format!(
                "vaulted {component} {version}, except {} which no repository offers any more",
                unobtainable.join(", ")
            )),
            Ok(_) => None,
        })
    }

    /// The version whose boot entry should be the default, if this component
    /// manages boot entries.
    fn expected_boot_version(&self, eval: &Evaluation) -> Option<Evr> {
        let cfg = self.config.components.get(&eval.component)?;
        if !cfg.boot_entries {
            return None;
        }
        let cstate = self.state.component(&eval.component);
        match cstate.lifecycle {
            Lifecycle::Testing => cstate.testing.clone(),
            _ => cstate.boot_pin.clone().or_else(|| eval.installed.clone()),
        }
    }

    /// Make the default boot entry match what each boot component is holding,
    /// and check that the known-good version still has an entry.
    ///
    /// The distribution's snapshot hook resets the default to the newest
    /// installed kernel after every transaction, so this runs after each one.
    pub fn enforce_boot_defaults(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        if !self.config.components.values().any(|c| c.boot_entries) {
            return Ok(notes);
        }
        let evals = self.evaluate(now)?;
        let Some(entries) = boot::entries(&self.config.boot, &mut self.runner).unwrap_or(None)
        else {
            if evals
                .iter()
                .any(|e| self.expected_boot_version(e).is_some())
            {
                notes.push(
                    "boot entries could not be read, so the default entry was not verified".into(),
                );
            }
            return Ok(notes);
        };

        for eval in &evals {
            let Some(expected) = self.expected_boot_version(eval) else {
                continue;
            };
            let current = entries.iter().find(|e| e.is_default);
            match boot::pick_entry(&entries, &expected) {
                None => notes.push(format!(
                    "{}: no boot entry for {expected}; the default entry was left alone",
                    eval.component
                )),
                Some(e) if e.is_default => {}
                Some(e) => {
                    boot::set_default(&mut self.runner, &e.id)?;
                    notes.push(format!(
                        "{}: default boot entry set to {} (was {})",
                        eval.component,
                        e.id,
                        current.map_or("unset", |c| c.id.as_str())
                    ));
                }
            }

            let cstate = self.state.component(&eval.component);
            if let Some(kg) = &cstate.known_good {
                let installed = eval.family.iter().any(|p| &p.evr == kg)
                    || self
                        .packages_installed_at(&eval.component, kg)
                        .unwrap_or(false);
                if installed && boot::pick_entry(&entries, kg).is_none() {
                    notes.push(format!(
                        "{}: the known-good version {kg} is installed but has no boot entry",
                        eval.component
                    ));
                }
            }
        }
        Ok(notes)
    }

    fn packages_installed_at(&mut self, component: &str, version: &Evr) -> Result<bool> {
        let cfg = self.config.component(component)?.clone();
        let (installed, _) = self.packages(&cfg)?;
        Ok(installed.iter().any(|p| &p.evr == version))
    }

    /// Why a reboot is due, if one is: the running kernel is not the one
    /// this machine is meant to boot (a newer fix is installed, or a promotion
    /// or rollback is waiting), or zypper says core libraries were updated.
    pub fn reboot_needed(&mut self, components: &[ComponentStatus]) -> Option<String> {
        let running = self.running_kernel.clone();
        for c in components {
            let Some(expected) = self.expected_boot_version(&c.eval) else {
                continue;
            };
            if let Some(running) = &running {
                if !boot::kernel_release_matches(running, &expected) {
                    return Some(format!(
                        "{} {expected} is installed; the machine is running {running}",
                        c.eval.component
                    ));
                }
            }
        }
        if self.backend.needs_reboot(&mut self.runner).unwrap_or(false) {
            return Some("core libraries or services were updated".into());
        }
        None
    }

    /// Whether the default boot entry has drifted off what `eval` is holding.
    /// Reads the EFI variable, so it works unprivileged.
    fn boot_drift(&self, eval: &Evaluation) -> Option<String> {
        let expected = self.expected_boot_version(eval)?;
        let id = boot::default_entry_from_efivars(&self.config.boot.efivars_dir)?;
        if boot::entry_id_boots(&id, &expected) {
            return None;
        }
        Some(format!(
            "the default boot entry is {id}, not {expected}; a package transaction probably reset it. \
             Run `sluice repair` before rebooting"
        ))
    }

    /// Record a version as known-good: vault its RPMs and pin it against purging.
    pub fn mark_good(
        &mut self,
        component: &str,
        version: Option<Evr>,
        now: DateTime<Utc>,
    ) -> Result<MarkGoodReport> {
        let mut notes: Vec<String> = self.repair_locks()?.into_iter().collect();
        let (cfg, eval) = self.eval_of(component, now)?;

        let version = version
            .or_else(|| eval.installed.clone())
            .with_context(|| format!("`{component}` has no installed version to mark"))?;
        anyhow::ensure!(
            self.packages_installed_at(component, &version)?,
            "{component} {version} is not installed; only an installed version can be vouched for"
        );

        notes.extend(self.vault_version(component, &cfg, &eval, &version)?);

        let cstate = self.state.component_mut(component);
        cstate.known_good = Some(version.clone());
        cstate.known_good_marked_at = Some(now);
        cstate.lifecycle = Lifecycle::KnownGood;
        cstate.boot_pin = None;
        if cstate.testing.as_ref() == Some(&version) {
            cstate.testing = None;
        }
        let vaulted = cstate.is_vaulted(&version);

        notes.extend(self.verify_retention()?);
        self.save()?;

        // Boot evidence is informational here; a missing journal must not
        // block marking a version good.
        let health = self
            .health_report()
            .ok()
            .and_then(|h| match_kernel_health(&h, &version));

        Ok(MarkGoodReport {
            component: component.to_string(),
            version,
            vaulted,
            health,
            notes,
        })
    }

    /// Make the known-good version the one the machine runs again.
    ///
    /// The tested version is left installed unless `remove` is set — the point
    /// is to get back to a working state, not to destroy the evidence — but it
    /// is put back behind the gate, and the boot default is held on the
    /// known-good version.
    pub fn rollback(
        &mut self,
        component: &str,
        remove: bool,
        now: DateTime<Utc>,
    ) -> Result<RollbackReport> {
        let mut steps: Vec<String> = self.repair_locks()?.into_iter().collect();
        let cstate = self.state.component(component);
        let known_good = cstate
            .known_good
            .clone()
            .with_context(|| format!("`{component}` has no known-good version to roll back to"))?;
        let (cfg, eval) = self.eval_of(component, now)?;
        let re = series_regex(&cfg.series_regex)?;

        let tested = cstate
            .testing
            .clone()
            .or_else(|| eval.installed.clone())
            .filter(|t| t != &known_good);

        if remove {
            if let (Some(t), Some(running)) = (&tested, &self.running_kernel) {
                anyhow::ensure!(
                    !(cfg.boot_entries && boot::kernel_release_matches(running, t)),
                    "{t} is the running kernel, and removing it would pull modules out from under \
                     this session. Run `sluice rollback {component}` without --remove, reboot into \
                     {known_good}, then remove it with `sluice rollback {component} --remove`."
                );
            }
        }

        let (installed, _) = self.packages(&cfg)?;
        let known_good_installed = installed.iter().any(|p| p.evr == known_good);

        // Reinstall the known-good version first if it is gone: pointing the
        // bootloader at a kernel that is not there leaves an unbootable machine.
        let to_install = if known_good_installed {
            Vec::new()
        } else {
            vault::ensure_registered(
                self.backend.as_ref(),
                &mut self.runner,
                &self.config.paths.vault_dir,
            )?;
            let targets = self.family_at_target(&cfg, &eval, &known_good)?;
            anyhow::ensure!(
                !targets.is_empty(),
                "{known_good} is neither installed nor obtainable from the vault or repositories; \
                 cannot roll back"
            );
            targets
        };
        let to_remove = match (&tested, remove) {
            (Some(t), true) => policy::resolve_family(&cfg, &installed, Some(t))
                .into_iter()
                .filter(|p| policy::same_build(&cfg, &p.evr, t) || policy::is_kmp(&p.name))
                .collect(),
            _ => Vec::new(),
        };

        if !to_install.is_empty() || !to_remove.is_empty() {
            let owned: Vec<LockSpec> = self
                .backend
                .locks(&mut self.runner)?
                .into_iter()
                .filter(|l| owns(l, &eval))
                .collect();
            let state_dir = self.config.paths.state_dir.clone();
            let window = gate::LockWindow::open(
                self.backend.as_ref(),
                &mut self.runner,
                &state_dir,
                owned,
                "rollback",
            )?;
            let installed_result = self.backend.install_exact(&mut self.runner, &to_install);
            let removed_result = match &installed_result {
                Ok(_) => self.backend.remove_exact(&mut self.runner, &to_remove),
                Err(_) => Ok(Default::default()),
            };
            window.close(&mut self.runner)?;
            installed_result?;
            removed_result?;

            if !to_install.is_empty() {
                steps.push(format!("reinstalled {known_good} from the vault"));
            }
            if !to_remove.is_empty() {
                steps.push(format!(
                    "removed {}",
                    to_remove
                        .iter()
                        .map(Pkg::nevra)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }

        let cstate = self.state.component_mut(component);
        cstate.lifecycle = Lifecycle::KnownGood;
        cstate.testing = None;
        if cfg.policy == Policy::HoldSeries {
            cstate.series = known_good.series(&re);
        } else {
            // No series gate to fall back behind, so hold it explicitly.
            cstate.held_at = Some(known_good.clone());
        }
        if cfg.boot_entries {
            cstate.boot_pin = Some(known_good.clone());
        }

        // Back behind the gate of the known-good series: a kept tested version
        // is now above the lock and frozen there.
        for change in self.reconcile_locks(now)? {
            steps.push(format!("lock: {change}"));
        }

        let boot_notes = self.enforce_boot_defaults(now)?;
        let note = boot_notes
            .iter()
            .find(|n| n.contains("could not") || n.contains("no boot entry"))
            .cloned();
        steps.extend(boot_notes.into_iter().filter(|n| Some(n) != note.as_ref()));
        let still_installed = match &tested {
            Some(t) => self.packages_installed_at(component, t)?,
            None => false,
        };
        if still_installed && !remove {
            steps.push(format!(
                "{} stays installed but gated; `sluice promote {component}` takes it again, \
                 `sluice rollback {component} --remove` uninstalls it",
                tested.as_ref().map(Evr::to_string).unwrap_or_default()
            ));
        }
        self.save()?;

        Ok(RollbackReport {
            component: component.to_string(),
            to: known_good,
            installed: true,
            steps,
            note,
        })
    }

    // -----------------------------------------------------------------------
    // evidence
    // -----------------------------------------------------------------------

    /// The packages whose history is worth keeping: every component's anchor.
    fn tracked_names(&self) -> std::collections::BTreeSet<String> {
        self.config
            .components
            .values()
            .filter_map(|c| c.anchor.clone())
            .collect()
    }

    /// What root runs have recorded, refreshed from zypp's history log when
    /// this process can read it.
    pub fn evidence(&self) -> crate::evidence::Evidence {
        let mut e = crate::evidence::Evidence::load(&self.config.paths.state_dir);
        if let Ok(text) = std::fs::read_to_string(&self.config.health.zypp_history) {
            e.history = crate::evidence::parse_zypp_history(&text, &self.tracked_names());
        }
        e
    }

    /// The boot-health report with every boot attributed to a kernel: named
    /// by its journal, recorded by an earlier root run, or inferred from the
    /// install history. A run with root also records what it saw.
    pub fn health_report(&mut self) -> Result<HealthReport> {
        let mut report = health::report(&self.config.health, &mut self.runner)?;
        let mut evidence = self.evidence();

        if crate::privilege::is_root() && !self.runner.dry_run() {
            let history = std::fs::read_to_string(&self.config.health.zypp_history)
                .ok()
                .map(|t| crate::evidence::parse_zypp_history(&t, &self.tracked_names()));
            evidence.merge(history, &report, Utc::now());
            if let Err(e) = evidence.save(&self.config.paths.state_dir) {
                self.runner.note(&format!("could not save evidence: {e:#}"));
            }
        }

        // Boots are attributed to the kernel component, if there is one.
        let kernel = self
            .config
            .components
            .iter()
            .find(|(_, c)| c.boot_entries)
            .map(|(n, c)| (n.clone(), c.clone()));
        if let Some((_, cfg)) = kernel {
            if let Some(anchor) = cfg.anchor.clone() {
                let installed_now: Vec<(Evr, Option<DateTime<Utc>>)> = self
                    .backend
                    .installed_details(&mut self.runner, std::slice::from_ref(&anchor))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.evr, p.installed_at))
                    .collect();
                crate::evidence::attribute(
                    &mut report,
                    &evidence,
                    &anchor,
                    self.running_kernel.as_deref(),
                    &installed_now,
                );
            }
        }
        Ok(report)
    }

    // -----------------------------------------------------------------------
    // timeline
    // -----------------------------------------------------------------------

    /// This machine's side of a component's timeline: which versions are
    /// installed and since when, which one is running, which one boots by
    /// default, what is known-good, under test, gated or vaulted, and — for a
    /// kernel — every boot and how it ended.
    pub fn machine(
        &mut self,
        component: &str,
        health: Option<&HealthReport>,
        now: DateTime<Utc>,
    ) -> Result<crate::timeline::Machine> {
        let (_, eval) = self.eval_of(component, now)?;
        self.machine_for(&eval, health)
    }

    /// [`machine`](Self::machine) for an evaluation already in hand, which
    /// saves re-querying every component.
    pub fn machine_for(
        &mut self,
        eval: &Evaluation,
        health: Option<&HealthReport>,
    ) -> Result<crate::timeline::Machine> {
        use crate::timeline::{normalize, BootSpan, Machine};

        let component = eval.component.as_str();
        let cfg = self.config.component(component)?.clone();
        let (installed, available) = self.packages(&cfg)?;
        let anchor = cfg
            .anchor
            .clone()
            .or_else(|| eval.family.first().map(|p| p.name.clone()));
        let details = self
            .backend
            .installed_details(
                &mut self.runner,
                &anchor.iter().cloned().collect::<Vec<_>>(),
            )
            .unwrap_or_default();
        let cstate = self.state.component(component);
        let default_id = cfg
            .boot_entries
            .then(|| boot::default_entry_from_efivars(&self.config.boot.efivars_dir))
            .flatten();

        let mut m = Machine {
            series: eval.series.clone(),
            ..Default::default()
        };
        let is_anchor = |p: &Pkg| Some(&p.name) == anchor.as_ref();

        for p in installed.iter().filter(|p| is_anchor(p)) {
            let marks = m.marks.entry(normalize(&p.evr.version)).or_default();
            marks.installed = true;
            marks.installed_at = details
                .iter()
                .find(|d| d.name == p.name && d.evr == p.evr)
                .and_then(|d| d.installed_at)
                .map(|t| t.date_naive());
            if cfg.boot_entries {
                marks.running |= self
                    .running_kernel
                    .as_deref()
                    .is_some_and(|k| boot::kernel_release_matches(k, &p.evr));
                marks.default_boot |= default_id
                    .as_deref()
                    .is_some_and(|id| boot::entry_id_boots(id, &p.evr));
            } else {
                // Userspace has one version installed, and that is what runs.
                marks.running = true;
            }
        }
        for p in available.iter().filter(|p| is_anchor(p) && p.offered()) {
            m.marks
                .entry(normalize(&p.evr.version))
                .or_default()
                .offered = true;
        }
        let mut set = |v: &Evr, f: fn(&mut crate::timeline::VersionMarks)| {
            f(m.marks.entry(normalize(&v.version)).or_default());
        };
        if let Some(v) = &eval.installed {
            set(v, |x| x.current = true);
        }
        if let Some(v) = &cstate.known_good {
            set(v, |x| x.known_good = true);
        }
        if let Some(v) = &cstate.testing {
            set(v, |x| x.testing = true);
        }
        if let Some((_, v)) = eval.gated_series() {
            set(v, |x| x.gated = true);
        }
        for v in &cstate.vault {
            set(&v.version, |x| x.vaulted = true);
        }

        // What was installed and removed, and when.
        if let Some(anchor) = &anchor {
            let evidence = self.evidence();
            for e in evidence.history.iter().filter(|e| &e.name == anchor) {
                let version = normalize(&e.evr.version);
                let removed = e.action == crate::evidence::Action::Remove;
                m.history.push(crate::timeline::Change {
                    at: e.at,
                    removed,
                    version: version.clone(),
                });
                let marks = m.marks.entry(version).or_default();
                if removed {
                    marks.removed_at = Some(e.at.date_naive());
                } else if marks.installed_at.is_none() {
                    marks.installed_at = Some(e.at.date_naive());
                }
            }
            // Removed and then reinstalled is simply installed.
            for marks in m.marks.values_mut().filter(|x| x.installed) {
                marks.removed_at = None;
            }
            m.history.sort_by_key(|c| c.at);
        }

        if cfg.boot_entries {
            if let Some(h) = health {
                m.boots = h
                    .boots
                    .iter()
                    .map(|b| BootSpan {
                        start: b.start,
                        end: b.end,
                        clean: b.clean_end,
                        kernel: b.kernel.clone(),
                        kernel_inferred: b.kernel_inferred,
                        pstore_hits: b.pstore_hits,
                    })
                    .collect();
                m.records = crate::evidence::per_version(&h.boots)
                    .into_iter()
                    .map(|(v, r)| (normalize(&v), r))
                    .collect();
            }
        }
        Ok(m)
    }

    /// Series a component's timeline must show whatever their age: the ones
    /// this machine has installed, holds, or is offered.
    pub fn timeline_keep(
        &mut self,
        component: &str,
        now: DateTime<Utc>,
    ) -> Result<std::collections::BTreeSet<String>> {
        let (_, eval) = self.eval_of(component, now)?;
        let machine = self.machine_for(&eval, None)?;
        self.timeline_keep_for(&eval, &machine)
    }

    /// [`timeline_keep`](Self::timeline_keep) from data already in hand.
    pub fn timeline_keep_for(
        &self,
        eval: &Evaluation,
        machine: &crate::timeline::Machine,
    ) -> Result<std::collections::BTreeSet<String>> {
        let cfg = self.config.component(&eval.component)?;
        let re = series_regex(&cfg.series_regex)?;
        let cstate = self.state.component(&eval.component);
        let mut keep: std::collections::BTreeSet<String> = machine
            .marks
            .iter()
            .filter(|(_, m)| m.installed)
            .filter_map(|(v, _)| Evr::parse(v).series(&re))
            .collect();
        keep.extend(eval.series.clone());
        keep.extend(cstate.known_good.as_ref().and_then(|v| v.series(&re)));
        keep.extend(eval.gated_series().map(|(s, _)| s.to_string()));
        Ok(keep)
    }

    // -----------------------------------------------------------------------
    // lineage
    // -----------------------------------------------------------------------

    pub fn lineage(
        &mut self,
        component: &str,
        series: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<LineageView> {
        let cfg = self.config.component(component)?.clone();
        let re = series_regex(&cfg.series_regex)?;
        let evals = self.evaluate(now)?;
        let eval = evals.iter().find(|e| e.component == component).cloned();
        let cstate = self.state.component(component);

        let mut fetcher = lineage::Fetcher::new(&self.config.lineage, &self.config.paths.cache_dir);
        let releases = fetcher.releases();

        let highlights: BTreeMap<String, Regex> = cfg
            .highlight
            .iter()
            .filter_map(|(k, v)| Regex::new(v).ok().map(|r| (k.clone(), r)))
            .collect();

        let current_series = eval.as_ref().and_then(|e| e.series.clone());
        // Asking for your own series by name shows it as yours, not as a
        // candidate.
        let candidate_series = series
            .map(str::to_string)
            .or_else(|| {
                eval.as_ref()
                    .and_then(|e| e.gated_series().map(|(s, _)| s.to_string()))
            })
            .filter(|s| Some(s) != current_series.as_ref());

        let max_points = self.config.lineage.max_points;
        let mut build = |s: &str, is_current: bool| -> SeriesLineage {
            let major = s.split('.').next().unwrap_or(s);
            let index = fetcher.index(major).unwrap_or_default();
            let status = releases
                .as_ref()
                .and_then(|r| lineage::series_status(r, &index, s, &re));

            let points = lineage::points_of(&index, s);
            let omitted = points.len().saturating_sub(max_points);
            let points: Vec<PointRelease> = points
                .into_iter()
                .skip(omitted)
                .map(|entry| {
                    let shape = fetcher
                        .changelog(&entry.version)
                        .map(|text| lineage::analyse_changelog(&text, &highlights))
                        .unwrap_or_default();
                    PointRelease {
                        version: entry.version.clone(),
                        date: Some(entry.date),
                        patches: shape.patches,
                        reverts: shape.reverts,
                        highlights: shape.highlights,
                    }
                })
                .collect();

            SeriesLineage {
                series: s.to_string(),
                status,
                points,
                omitted,
                hint: None,
                repo_note: None,
                repo_changelog: Vec::new(),
                withdrawn: is_current && eval.as_ref().is_some_and(|e| e.series_withdrawn),
                is_current,
                gated: !is_current
                    && eval
                        .as_ref()
                        .is_some_and(|e| e.gated_series().is_some_and(|(series, _)| series == s)),
            }
        };

        let mut candidate = candidate_series.as_deref().map(|s| build(s, false));
        let current = current_series
            .as_deref()
            .filter(|s| Some(*s) != candidate_series.as_deref())
            .map(|s| build(s, true));

        if let Some(c) = &mut candidate {
            c.hint = c.hint(&cfg, &re);
            // The distribution's side: which build is waiting, and since when.
            // First-seen is the closest thing to "which snapshot introduced it".
            if let Some(g) = cstate.gated.as_ref().filter(|g| g.series == c.series) {
                let days = (now - g.first_seen).num_days();
                c.repo_note = Some(format!(
                    "the repositories have offered {} since {} ({days} day(s))",
                    g.version,
                    g.first_seen.format("%Y-%m-%d")
                ));
            }
        }

        let rpm_cache = fetcher.cache_dir().join("rpms");
        let mut view = LineageView {
            candidate,
            current,
            fetched_at: fetcher.oldest_fetch,
            from_cache: fetcher.used_cache,
            warnings: fetcher.warnings,
        };

        // The distribution's side, when asked for: its own changelog for the
        // candidate. This is a download, so it is opt-in.
        if self.config.lineage.repo_changelog && !self.config.lineage.offline {
            if let (Some(c), Some(eval)) = (view.candidate.as_mut(), eval.as_ref()) {
                if let Some((_, target)) = eval.gated_series().filter(|(s, _)| *s == c.series) {
                    let target = target.clone();
                    match self.candidate_changelog(component, &cfg, eval, &target, &rpm_cache) {
                        Ok(entries) => c.repo_changelog = entries,
                        Err(e) => view.warnings.push(format!("repository changelog: {e:#}")),
                    }
                }
            }
        }
        Ok(view)
    }

    fn candidate_changelog(
        &mut self,
        component: &str,
        cfg: &ComponentConfig,
        eval: &Evaluation,
        target: &Evr,
        rpm_cache: &std::path::Path,
    ) -> Result<Vec<String>> {
        let anchor = self
            .family_at_target(cfg, eval, target)?
            .into_iter()
            .filter(|p| p.offered())
            .find(|p| Some(&p.name) == cfg.anchor.as_ref())
            .with_context(|| format!("the repositories do not offer {component} {target}"))?;
        let dest = rpm_cache.join(component).join(target.to_string());
        let files =
            self.backend
                .fetch_rpms(&mut self.runner, std::slice::from_ref(&anchor), &dest)?;
        let Some(file) = files.first() else {
            return Ok(Vec::new());
        };
        Ok(self
            .backend
            .file_changelog(&mut self.runner, file)?
            .map(|text| lineage::newest_rpm_entries(&text, 3))
            .unwrap_or_default())
    }

    /// Copy RPMs already downloaded for the changelog view into the vault, so
    /// vaulting that version does not download them again.
    fn seed_vault_from_cache(&self, component: &str, version: &Evr) {
        if self.runner.dry_run() {
            return;
        }
        let (cache, _) = lineage::resolve_cache_dir(self.config.paths.cache_dir.clone());
        let from = cache.join("rpms").join(component).join(version.to_string());
        let to = vault::component_dir(&self.config.paths.vault_dir, component, version);
        let Ok(walk) = walk_files(&from) else { return };
        for file in walk {
            if let Ok(rel) = file.strip_prefix(&from) {
                let dest = to.join(rel);
                if dest.exists() {
                    continue;
                }
                if let Some(dir) = dest.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::hard_link(&file, &dest)
                    .or_else(|_| std::fs::copy(&file, &dest).map(|_| ()));
            }
        }
    }

    // -----------------------------------------------------------------------
    // check
    // -----------------------------------------------------------------------

    /// Non-interactive: detect what needs attention and notify. Changes nothing
    /// about the system, but does record what has been announced so a daily
    /// timer does not repeat itself.
    pub fn check(&mut self, now: DateTime<Utc>, force: bool) -> Result<CheckReport> {
        if self.config.backend.refresh {
            let _ = self.backend.refresh(&mut self.runner);
        }
        let status = self.status(now)?;

        let mut alerts: Vec<(Urgency, String)> = Vec::new();
        let mut announced: Vec<(String, Evr)> = Vec::new();

        for c in &status.components {
            let cfg = self.config.component(&c.eval.component)?;
            if let Some((series, target)) = c.eval.gated_series() {
                self.state
                    .component_mut(&c.eval.component)
                    .record_gated(series, target, now);
            }
            for (urgency, msg) in c.alerts(cfg) {
                // A gated series is announced once. Everything else is a
                // condition that persists and is worth repeating.
                if let Some((_, target)) = c.eval.gated_series() {
                    if urgency == Urgency::Info {
                        let already = self
                            .state
                            .component(&c.eval.component)
                            .gated
                            .as_ref()
                            .is_some_and(|g| &g.version == target && g.notified_at.is_some());
                        if already && !force {
                            continue;
                        }
                        announced.push((c.eval.component.clone(), target.clone()));
                    }
                }
                alerts.push((urgency, format!("{}: {}", c.eval.component, msg)));
            }
        }

        // A boot that ended without a shutdown is news once. Only boots that
        // ended since the last check are reported; the first check looks back
        // a week rather than replaying the whole journal.
        let since = if force {
            None
        } else {
            Some(self.state.last_check.unwrap_or(now - Duration::days(7)))
        };
        for b in status.health.unclean() {
            if since.is_some_and(|t| b.end <= t) {
                continue;
            }
            let mut msg = format!(
                "a boot ended without a clean shutdown: {} ({})",
                health::local_window(b),
                b.kernel.as_deref().unwrap_or("kernel unknown")
            );
            if b.pstore_hits > 0 {
                msg.push_str(&format!(", {} pstore crash record(s)", b.pstore_hits));
            }
            alerts.push((Urgency::Warning, msg));
        }

        // A failed update is news once; a machine that has not updated
        // successfully in two weeks is a condition, and is repeated.
        let mut announce_failure = None;
        if let Some(run) = self.state.last_update_run().cloned() {
            if !run.ok && (force || self.state.announced_failure != Some(run.started)) {
                let why = run.error.as_deref().unwrap_or("unknown error");
                alerts.push((
                    Urgency::Warning,
                    format!(
                        "the update on {} failed: {}",
                        run.started
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M"),
                        why.lines().next().unwrap_or(why)
                    ),
                ));
                announce_failure = Some(run.started);
            }
            let since = self.state.last_successful_update().map(|r| r.finished);
            let stale = match since {
                Some(t) => now - t > Duration::days(14),
                None => true,
            };
            if !run.ok && stale {
                alerts.push((
                    Urgency::Warning,
                    match since {
                        Some(t) => format!(
                            "no update has succeeded since {}",
                            t.with_timezone(&chrono::Local).format("%Y-%m-%d")
                        ),
                        None => "no update has succeeded yet".into(),
                    },
                ));
            }
        }
        let mut announce_reboot = None;
        if let Some(reason) = &status.reboot_needed {
            if force || self.state.announced_reboot.as_deref() != Some(reason.as_str()) {
                alerts.push((
                    Urgency::Info,
                    format!("reboot to finish updating: {reason}"),
                ));
                announce_reboot = Some(reason.clone());
            }
        }

        // A new sluice is announced once, like a gated series.
        let mut announce_release = None;
        if let Some(r) = &status.new_release {
            let already = self.state.announced_release.as_deref() == Some(r.version());
            if force || !already {
                alerts.push((
                    Urgency::Info,
                    format!(
                        "sluice {} is available (this is {}); `sudo sluice self-update` installs it",
                        r.version(),
                        crate::selfupdate::CURRENT
                    ),
                ));
                announce_release = Some(r.version().to_string());
            }
        }

        if status.esp_low {
            if let Some(esp) = &status.esp {
                alerts.push((
                    Urgency::Warning,
                    format!(
                        "ESP {} has {} MB free of {} MB ({}% used); new kernels may fail to install",
                        esp.path.display(),
                        esp.free_mb,
                        esp.total_mb,
                        esp.used_percent()
                    ),
                ));
            }
        }

        let mut problems = Vec::new();
        if !alerts.is_empty() {
            let urgency = alerts
                .iter()
                .map(|(u, _)| *u)
                .max()
                .unwrap_or(Urgency::Info);
            let body = alerts
                .iter()
                .map(|(_, m)| format!("• {m}"))
                .collect::<Vec<_>>()
                .join("\n");
            problems = notify::send(
                &self.config.notify,
                &mut self.runner,
                &Notification::new("sluice: updates need your attention", body, urgency),
            );

            if let Some(v) = announce_release {
                self.state.announced_release = Some(v);
            }
            if let Some(t) = announce_failure {
                self.state.announced_failure = Some(t);
            }
            if announce_reboot.is_some() {
                self.state.announced_reboot = announce_reboot;
            }
            for (component, version) in announced {
                let cstate = self.state.component_mut(&component);
                if let Some(g) = &mut cstate.gated {
                    if g.version == version {
                        g.notified_at = Some(now);
                    }
                }
            }
        }

        self.state.last_check = Some(now);
        let mut notes = Vec::new();
        if let Err(e) = self.save() {
            // `check` is a read-only command and must work unprivileged; it just
            // cannot remember what it has already said.
            if crate::privilege::is_root() {
                return Err(e);
            }
            notes.push(format!(
                "not running as root, so what was announced is not remembered ({e:#}); \
                 the same alerts will repeat next time"
            ));
        }

        Ok(CheckReport {
            alerts,
            notify_problems: problems,
            notes,
            status,
        })
    }

    // -----------------------------------------------------------------------
    // migrate
    // -----------------------------------------------------------------------

    /// Components `migrate` records a known-good version for, with the
    /// versions installed for each, so the caller can ask which one.
    pub fn migrate_candidates(&mut self, now: DateTime<Utc>) -> Result<Vec<(String, Vec<Evr>)>> {
        let mut out = Vec::new();
        let gating: Vec<(String, ComponentConfig)> = self
            .config
            .components
            .iter()
            .filter(|(_, c)| matches!(c.policy, Policy::HoldSeries | Policy::Hold))
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        let _ = now;
        for (name, cfg) in gating {
            let (installed, _) = self.packages(&cfg)?;
            let anchor = cfg.anchor.clone().or_else(|| {
                policy::resolve_family(&cfg, &installed, None)
                    .first()
                    .map(|p| p.name.clone())
            });
            let mut versions: Vec<Evr> = installed
                .iter()
                .filter(|p| Some(&p.name) == anchor.as_ref())
                .map(|p| p.evr.clone())
                .collect();
            versions.sort();
            versions.dedup();
            out.push((name, versions));
        }
        Ok(out)
    }

    /// Adopt an existing manual setup: replace blanket locks with series gates,
    /// record and vault a known-good version for each gated component, and
    /// remember what was there before so `--undo` can put it back exactly.
    ///
    /// `known_good` maps component names to the version to record. A gated
    /// component left out of it gets its newest installed version.
    pub fn migrate(
        &mut self,
        known_good: &BTreeMap<String, Evr>,
        now: DateTime<Utc>,
    ) -> Result<MigrateReport> {
        anyhow::ensure!(
            self.state.migration.is_none(),
            "this system has already been migrated (on {}). Use `sluice migrate --undo` first.",
            self.state
                .migration
                .as_ref()
                .map(|m| m.at.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        );
        let mut notes: Vec<String> = self.repair_locks()?.into_iter().collect();

        let candidates = self.migrate_candidates(now)?;
        for name in known_good.keys() {
            anyhow::ensure!(
                candidates.iter().any(|(c, _)| c == name),
                "`{name}` is not a component with a hold-series or hold policy, so it has no known-good version to record"
            );
        }

        let prior_locks = self.backend.locks(&mut self.runner)?;
        let prior_multiversion = boot::read_multiversion(&self.config.boot.package_manager_conf)
            .ok()
            .flatten()
            .map(|m| m.render());
        let prior_components = self.state.components.clone();

        for (name, installed) in &candidates {
            let (cfg, eval) = self.eval_of(name, now)?;
            let Some(version) = known_good
                .get(name)
                .cloned()
                .or_else(|| eval.installed.clone())
            else {
                notes.push(format!(
                    "{name}: nothing installed, so no known-good version recorded"
                ));
                continue;
            };
            anyhow::ensure!(
                installed.contains(&version),
                "{name} {version} is not installed (installed: {})",
                installed
                    .iter()
                    .map(Evr::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            let cstate = self.state.component_mut(name);
            cstate.known_good = Some(version.clone());
            cstate.known_good_marked_at = Some(now);
            cstate.lifecycle = Lifecycle::KnownGood;
            // The series held is the one you are on, which need not be the
            // known-good one: 7.2 keeps flowing even with 7.0.12 as the fallback.
            cstate.series = eval.series.clone();
            notes.push(format!("recorded {name} {version} as known-good"));

            match self.vault_version(name, &cfg, &eval, &version)? {
                Some(problem) => notes.push(format!("WARNING: {problem}")),
                None => notes.push(format!("vaulted {name} {version}")),
            }
        }

        let lock_changes = self.reconcile_locks(now)?;
        notes.extend(self.verify_retention()?);

        self.state.migration = Some(MigrationRecord {
            at: now,
            prior_locks: prior_locks.clone(),
            prior_multiversion,
            prior_components,
        });
        self.save()?;

        Ok(MigrateReport {
            prior_locks,
            lock_changes,
            notes,
            undone: false,
        })
    }

    pub fn migrate_undo(&mut self, _now: DateTime<Utc>) -> Result<MigrateReport> {
        let record = self
            .state
            .migration
            .clone()
            .context("this system has not been migrated, so there is nothing to undo")?;

        let current = self.backend.locks(&mut self.runner)?;
        let mut lock_changes = Vec::new();

        // Remove only what sluice added, then restore exactly what was there.
        for lock in &current {
            let ours = lock
                .comment
                .as_deref()
                .is_some_and(|c| c.starts_with("sluice:"));
            if ours && !record.prior_locks.contains(lock) {
                self.backend.remove_lock(&mut self.runner, lock)?;
                lock_changes.push(format!("- lock {}", lock.spec_string()));
            }
        }
        for lock in &record.prior_locks {
            if !current.contains(lock) {
                self.backend.add_lock(&mut self.runner, lock)?;
                lock_changes.push(format!("+ lock {}", lock.spec_string()));
            }
        }

        let mut notes = Vec::new();
        if let Some(prior) = &record.prior_multiversion {
            let mv = boot::Multiversion::parse(prior);
            match boot::write_multiversion(
                &self.config.boot.package_manager_conf,
                &mv,
                self.runner.dry_run(),
            ) {
                Ok(true) => notes.push(format!("restored multiversion.kernels = {prior}")),
                Ok(false) => {}
                Err(e) => notes.push(format!("could not restore multiversion.kernels: {e:#}")),
            }
            self.state.pinned.retain(|p| mv.contains(&p.to_string()));
        }

        // Known-good records and lifecycle go back to how they were. Vaulted
        // RPMs are kept: they are harmless, and expensive to fetch again.
        let mut restored = record.prior_components.clone();
        for (name, cstate) in &self.state.components {
            let prior = restored.entry(name.clone()).or_default();
            prior.vault = cstate.vault.clone();
        }
        self.state.components = restored;
        self.state.migration = None;
        self.save()?;

        Ok(MigrateReport {
            prior_locks: record.prior_locks,
            lock_changes,
            notes,
            undone: true,
        })
    }
}

fn walk_files(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// A lock belongs to a component if sluice wrote it, or if it is a blanket
/// lock on one of the component's packages — the shape the user's manual setup
/// leaves behind, and exactly what `migrate` replaces.
fn owns(lock: &LockSpec, eval: &Evaluation) -> bool {
    let marker = format!("sluice: {} ", eval.component);
    if lock
        .comment
        .as_deref()
        .is_some_and(|c| c.starts_with(&marker))
    {
        return true;
    }
    lock.is_blanket() && eval.family_names().contains(&lock.name)
}

/// The locks a component's policy calls for.
///
/// Every family member sharing the component's version is locked, not just the
/// anchor, or `zypper dup` could move `kernel-devel` into a series
/// `kernel-default` is held out of. KMPs are left alone: they are versioned by
/// their own upstream, and one built for a held kernel cannot install anyway.
fn desired_locks(component: &str, cfg: &ComponentConfig, eval: &Evaluation) -> Vec<LockSpec> {
    let mut names: Vec<String> = eval
        .family
        .iter()
        .filter(|p| {
            !policy::is_kmp(&p.name)
                && eval
                    .installed
                    .as_ref()
                    .is_some_and(|i| policy::same_build(cfg, &p.evr, i))
        })
        .map(|p| p.name.clone())
        .collect();
    if names.is_empty() {
        names.extend(cfg.anchor.clone());
    }
    names.sort();
    names.dedup();

    if let Some(held) = &eval.held_at {
        return names
            .into_iter()
            .map(|n| {
                LockSpec::bounded(n, crate::backend::CmpOp::Gt, held.clone())
                    .with_comment(format!("sluice: {component} rollback hold"))
            })
            .collect();
    }

    match cfg.policy {
        Policy::Follow => Vec::new(),
        Policy::Hold => names
            .into_iter()
            .map(|n| LockSpec::blanket(n).with_comment(format!("sluice: {component} hold")))
            .collect(),
        Policy::HoldSeries => {
            let Some(next) = eval.series.as_deref().and_then(gate::next_series) else {
                return Vec::new();
            };
            names
                .iter()
                .map(|n| gate::series_lock(n, &next, component))
                .collect()
        }
        Policy::Soak => {
            // Nothing newer than what has finished soaking may install, so the
            // bound is the settled target, or the installed version until one is.
            let ceiling = match &eval.decision {
                Decision::Apply { target } => Some(target.clone()),
                _ => eval.installed.clone(),
            };
            let Some(ceiling) = ceiling else {
                return Vec::new();
            };
            names
                .into_iter()
                .map(|n| {
                    LockSpec::bounded(n, crate::backend::CmpOp::Gt, ceiling.clone())
                        .with_comment(format!("sluice: {component} soak"))
                })
                .collect()
        }
    }
}

/// Match a package version like `7.2.6-1.1` against a kernel uname like
/// `7.2.6-1-default`. The release component differs by design, so only the
/// upstream version is compared.
fn match_kernel_health(report: &HealthReport, installed: &Evr) -> Option<KernelHealth> {
    report.by_kernel().into_iter().find(|k| {
        k.kernel.starts_with(&format!("{}-", installed.version)) || k.kernel == installed.version
    })
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub dup_ran: bool,
    /// zypper's one-line summary of the upgrade.
    pub summary: Option<String>,
    /// Why a reboot is now due, if one is.
    pub reboot_needed: Option<String>,
    pub applied: Vec<(String, Evr)>,
    pub gated: Vec<(String, String, Evr)>,
    pub soaking: Vec<(String, Evr, DateTime<Utc>)>,
    pub held: Vec<(String, Evr)>,
    pub lock_changes: Vec<String>,
    pub notes: Vec<String>,
}

impl UpdateReport {
    pub fn changed_anything(&self) -> bool {
        !self.applied.is_empty() || !self.lock_changes.is_empty()
    }
}

#[derive(Debug)]
pub struct PromoteReport {
    pub component: String,
    pub from: Option<Evr>,
    pub to: Evr,
    pub series: String,
    pub packages: Vec<String>,
    pub notes: Vec<String>,
}

/// See [`App::promote_preview`].
#[derive(Debug, Clone, Default)]
pub struct PromotePreview {
    pub component: String,
    pub from: Option<Evr>,
    pub to: Option<Evr>,
    /// The new series, when crossing a gate (not when releasing a hold).
    pub series: Option<String>,
    pub packages: Vec<String>,
    pub known_good: Option<Evr>,
    pub known_good_vaulted: bool,
    pub gate_now: Vec<String>,
    pub gate_after: Vec<String>,
    /// Reasons it cannot go ahead.
    pub blockers: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct MarkGoodReport {
    pub component: String,
    pub version: Evr,
    pub vaulted: bool,
    pub health: Option<KernelHealth>,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct RollbackReport {
    pub component: String,
    pub to: Evr,
    pub installed: bool,
    pub steps: Vec<String>,
    pub note: Option<String>,
}

pub struct CheckReport {
    pub alerts: Vec<(Urgency, String)>,
    pub notify_problems: Vec<String>,
    pub notes: Vec<String>,
    pub status: Status,
}

#[derive(Debug)]
pub struct MigrateReport {
    pub prior_locks: Vec<LockSpec>,
    pub lock_changes: Vec<String>,
    pub notes: Vec<String>,
    pub undone: bool,
}

/// How long a series has been EOL, for display.
pub fn eol_age(days: i64) -> String {
    match days {
        d if d < 7 => format!("{d} day(s)"),
        d if d < 60 => format!("{} week(s)", d / 7),
        d => format!("{} month(s)", d / 30),
    }
}

/// Convenience for the CLI's `--since` style options.
pub fn days_ago(now: DateTime<Utc>, days: i64) -> DateTime<Utc> {
    now - Duration::days(days)
}
