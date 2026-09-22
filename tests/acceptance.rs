//! The Phase 1 acceptance tests from the specification, run end to end against
//! the in-memory backend.
//!
//! These describe the situation the tool was written for: a machine installed
//! at kernel 7.2.0 with a blanket `kernel-default` lock, a repository that has
//! moved on to 7.2.6, and a 7.3 series waiting on the other side of the gate.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use sluice::app::App;
use sluice::backend::mock::MockBackend;
use sluice::backend::{LockSpec, PackageBackend, Pkg, PkgStatus};
use sluice::config::{ComponentConfig, Config, Policy};
use sluice::exec::Runner;
use sluice::policy::Decision;
use sluice::state::Lifecycle;
use sluice::version::Evr;

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
}

fn pkg(name: &str, evr: &str, installed: bool) -> Pkg {
    Pkg {
        name: name.into(),
        evr: Evr::parse(evr),
        arch: "x86_64".into(),
        repo: if installed {
            "(System Packages)".into()
        } else {
            "repo-oss".into()
        },
        status: if installed {
            PkgStatus::Installed
        } else {
            PkgStatus::Available
        },
        source: None,
    }
}

/// An installed package that a repository also still carries.
fn offered(name: &str, evr: &str, installed: bool) -> Pkg {
    Pkg {
        repo: "repo-oss".into(),
        ..pkg(name, evr, installed)
    }
}

fn now() -> DateTime<Utc> {
    "2026-09-22T12:00:00Z".parse().unwrap()
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let mut config = Config::default();
        config.paths.state_dir = root.join("state");
        config.paths.cache_dir = root.join("cache");
        config.paths.vault_dir = root.join("vault");
        config.paths.log_file = root.join("sluice.log");
        config.boot.package_manager_conf = root.join("zypp.conf");
        config.boot.esp_path = root.join("esp");
        // Never touch the real bootloader or EFI variables from a test.
        config.boot.manager = sluice::config::BootManager::None;
        config.boot.efivars_dir = root.join("efivars");
        config.lineage.offline = true;
        config.notify.desktop = false;
        // Hermetic: never read the host's journal. `true` prints nothing,
        // which reads as an empty boot list.
        config.health.journalctl = "true".into();
        config.backend.refresh = false;

        std::fs::write(
            &config.boot.package_manager_conf,
            "[main]\nmultiversion = provides:multiversion(kernel)\nmultiversion.kernels = latest,latest-1,running\n",
        )
        .unwrap();

        let mut components = BTreeMap::new();
        components.insert(
            "kernel".to_string(),
            ComponentConfig {
                policy: Policy::HoldSeries,
                anchor: Some("kernel-default".into()),
                promote_hint_min_point: Some(3),
                boot_entries: true,
                ..Default::default()
            },
        );
        config.components = components;

        Fixture { _dir: dir, config }
    }

    /// A machine held by a blanket lock: 7.2.0 and 7.0.12 installed, 7.2.6 available.
    fn tumbleweed_on_7_2(&self) -> MockBackend {
        MockBackend::with_packages(vec![
            pkg("kernel-default", "7.0.12-1.1", true),
            pkg("kernel-default", "7.2.0-1.1", true),
            pkg("kernel-devel", "7.2.0-1.1", true),
            pkg("kernel-default", "7.2.6-1.1", false),
            pkg("kernel-devel", "7.2.6-1.1", false),
            // Date-versioned firmware, which must never join the kernel family.
            pkg("kernel-firmware-amdgpu", "20260829-1.1", true),
        ])
    }

    /// The repository after Tumbleweed has jumped to 7.3.
    fn tumbleweed_on_7_3(&self) -> MockBackend {
        MockBackend::with_packages(vec![
            pkg("kernel-default", "7.2.6-1.1", true),
            pkg("kernel-devel", "7.2.6-1.1", true),
            pkg("kernel-default", "7.3.3-1.1", false),
            pkg("kernel-devel", "7.3.3-1.1", false),
        ])
    }

    fn app(&self, backend: MockBackend) -> App {
        App::new(self.config.clone(), false)
            .unwrap()
            .with_backend(Box::new(backend))
    }
}

/// `update` moves 7.2.0 to the latest Tumbleweed 7.2.x automatically.
#[test]
fn update_applies_a_point_release_without_asking() {
    let f = Fixture::new();
    let mut app = f.app(f.tumbleweed_on_7_2());

    let report = app.update(now()).unwrap();

    assert_eq!(report.applied.len(), 1);
    let (component, version) = &report.applied[0];
    assert_eq!(component, "kernel");
    assert_eq!(version.to_string(), "7.2.6-1.1");
    assert!(report.gated.is_empty());
    assert!(
        report.dup_ran,
        "the rest of the system still upgrades normally"
    );
}

/// `update` refuses a 7.3.x candidate and reports it as gated.
#[test]
fn update_refuses_a_new_series_and_reports_it() {
    let f = Fixture::new();
    let mut app = f.app(f.tumbleweed_on_7_3());

    let report = app.update(now()).unwrap();

    assert!(
        report.applied.is_empty(),
        "a series change must not be applied"
    );
    assert_eq!(report.gated.len(), 1);
    let (component, series, version) = &report.gated[0];
    assert_eq!(component, "kernel");
    assert_eq!(series, "7.3");
    assert_eq!(version.to_string(), "7.3.3-1.1");
}

/// The gate is a version-conditional lock, so point releases inside the held
/// series are never locked and no unlock window is ever opened.
#[test]
fn the_series_gate_is_expressed_as_a_bounded_lock() {
    let f = Fixture::new();
    let backend = MockBackend::with_packages(vec![
        pkg("kernel-default", "7.2.0-1.1", true),
        pkg("kernel-default", "7.2.6-1.1", false),
    ]);
    // The blanket lock the user set by hand, which migrate/update replaces.
    let mut r = Runner::new(false, None);
    backend
        .add_lock(&mut r, &LockSpec::blanket("kernel-default"))
        .unwrap();

    let mut app = f.app(backend);
    app.update(now()).unwrap();

    let locks = app.backend.locks(&mut app.runner).unwrap();
    let specs: Vec<String> = locks.iter().map(LockSpec::spec_string).collect();
    assert_eq!(specs, vec!["kernel-default >= 7.3"]);
    assert!(
        locks.iter().all(|l| !l.is_blanket()),
        "the blanket lock must be gone; it is what froze the machine"
    );
}

fn lock_specs(app: &mut App) -> Vec<String> {
    app.backend
        .locks(&mut app.runner)
        .unwrap()
        .iter()
        .map(LockSpec::spec_string)
        .collect()
}

/// What Tumbleweed does when it moves to 7.3: the 7.2 builds leave the
/// repository (installed copies become "System Packages") and 7.3.3 arrives.
fn tumbleweed_jumps_to_7_3(backend: &MockBackend) {
    let mut pkgs = backend.packages.borrow_mut();
    pkgs.retain(|p| p.installed());
    for p in pkgs.iter_mut() {
        p.repo = "(System Packages)".into();
    }
    pkgs.push(pkg("kernel-default", "7.3.3-1.1", false));
    pkgs.push(pkg("kernel-devel", "7.3.3-1.1", false));
}

/// promote → rollback → promote again, end to end, the way it happens: the
/// known-good version is marked while the repository still carries it.
#[test]
fn the_promotion_lifecycle_runs_end_to_end() {
    let f = Fixture::new();
    let backend = std::rc::Rc::new(f.tumbleweed_on_7_2());
    let mut app = App::new(f.config.clone(), false)
        .unwrap()
        .with_backend(Box::new(std::rc::Rc::clone(&backend)));

    // update moves to 7.2.6 but vouches for nothing: known-good is only ever
    // your decision.
    app.update(now()).unwrap();
    assert!(app.state.component("kernel").known_good.is_none());
    assert_eq!(app.state.component("kernel").lifecycle, Lifecycle::Unvetted);

    let marked = app.mark_good("kernel", None, now()).unwrap();
    assert_eq!(marked.version.to_string(), "7.2.6-1.1");
    assert!(
        marked.vaulted,
        "mark-good must vault, or rollback is not possible"
    );
    let mv = sluice::boot::read_multiversion(&app.config.boot.package_manager_conf)
        .unwrap()
        .unwrap();
    assert!(mv.contains("7.2.6-1.1"), "got: {}", mv.render());

    tumbleweed_jumps_to_7_3(&backend);

    let promoted = app.promote("kernel", now()).unwrap();
    assert_eq!(promoted.to.to_string(), "7.3.3-1.1");
    assert_eq!(promoted.series, "7.3");
    let state = app.state.component("kernel");
    assert_eq!(
        state.lifecycle,
        Lifecycle::Testing,
        "a promotion is never known-good"
    );
    assert_eq!(state.known_good.unwrap().to_string(), "7.2.6-1.1");
    assert_eq!(
        lock_specs(&mut app),
        vec!["kernel-default >= 7.4", "kernel-devel >= 7.4"],
        "the gate moved with us: 7.3 fixes flow, 7.4 is held"
    );

    // Roll back, keeping the tested kernel installed.
    let rolled = app.rollback("kernel", false, now()).unwrap();
    assert_eq!(rolled.to.to_string(), "7.2.6-1.1");
    let state = app.state.component("kernel");
    assert_eq!(state.lifecycle, Lifecycle::KnownGood);
    assert!(state.testing.is_none());
    assert_eq!(state.series.as_deref(), Some("7.2"));
    assert!(
        backend.removals.borrow().is_empty(),
        "nothing is removed without --remove"
    );

    // Back behind the 7.2 gate: 7.3.3 stays installed but frozen, and shows as
    // gated again rather than as the installed version.
    assert_eq!(
        lock_specs(&mut app),
        vec!["kernel-default >= 7.3", "kernel-devel >= 7.3"]
    );
    let status = app.status(now()).unwrap();
    let kernel = &status.components[0];
    assert_eq!(
        kernel.eval.installed.as_ref().unwrap().to_string(),
        "7.2.6-1.1"
    );
    assert!(
        matches!(&kernel.eval.decision, Decision::Gated { target, .. } if target.to_string() == "7.3.3-1.1"),
        "got {:?}",
        kernel.eval.decision
    );

    // And the tested version can be taken again without fetching anything.
    let again = app.promote("kernel", now()).unwrap();
    assert_eq!(again.to.to_string(), "7.3.3-1.1");
}

/// `rollback --remove` uninstalls the tested family, but never the running kernel.
#[test]
fn rollback_remove_uninstalls_the_tested_version_but_not_the_running_one() {
    let f = Fixture::new();
    let backend = std::rc::Rc::new(f.tumbleweed_on_7_2());
    let mut app = App::new(f.config.clone(), false)
        .unwrap()
        .with_backend(Box::new(std::rc::Rc::clone(&backend)));
    app.update(now()).unwrap();
    app.mark_good("kernel", None, now()).unwrap();
    tumbleweed_jumps_to_7_3(&backend);
    app.promote("kernel", now()).unwrap();

    app.running_kernel = Some("7.3.3-1-default".into());
    let err = app.rollback("kernel", true, now()).unwrap_err().to_string();
    assert!(err.contains("running kernel"), "got: {err}");
    assert_eq!(app.state.component("kernel").lifecycle, Lifecycle::Testing);

    app.running_kernel = Some("7.2.6-1-default".into());
    let rolled = app.rollback("kernel", true, now()).unwrap();
    assert!(
        rolled.steps.iter().any(|s| s.starts_with("removed")),
        "{:?}",
        rolled.steps
    );
    let removed = backend.removals.borrow().clone();
    assert_eq!(
        removed,
        vec![vec![
            "kernel-default=7.3.3-1.1".to_string(),
            "kernel-devel=7.3.3-1.1".to_string()
        ]]
    );
}

/// A rollback inside a series holds the boot default on the known-good version
/// until a newer fix arrives.
#[test]
fn a_within_series_rollback_pins_boot_until_the_next_fix() {
    let f = Fixture::new();
    let backend = std::rc::Rc::new(f.tumbleweed_on_7_2());
    let mut app = App::new(f.config.clone(), false)
        .unwrap()
        .with_backend(Box::new(std::rc::Rc::clone(&backend)));
    app.update(now()).unwrap();
    app.mark_good("kernel", Some(Evr::parse("7.2.6-1.1")), now())
        .unwrap();

    backend
        .packages
        .borrow_mut()
        .push(pkg("kernel-default", "7.2.7-1.1", false));
    backend
        .packages
        .borrow_mut()
        .push(pkg("kernel-devel", "7.2.7-1.1", false));
    app.update(now()).unwrap();
    assert_eq!(
        app.status(now()).unwrap().components[0]
            .eval
            .installed
            .as_ref()
            .unwrap()
            .to_string(),
        "7.2.7-1.1"
    );

    // 7.2.7 regressed.
    app.rollback("kernel", false, now()).unwrap();
    assert_eq!(
        app.state
            .component("kernel")
            .boot_pin
            .map(|v| v.to_string())
            .as_deref(),
        Some("7.2.6-1.1")
    );

    // The next fix clears the pin: fixes inside the series flow again.
    backend
        .packages
        .borrow_mut()
        .push(pkg("kernel-default", "7.2.8-1.1", false));
    backend
        .packages
        .borrow_mut()
        .push(pkg("kernel-devel", "7.2.8-1.1", false));
    app.update(now()).unwrap();
    assert!(app.state.component("kernel").boot_pin.is_none());
}

/// Marking a newer version good releases the older pin sluice added, but never
/// one you added by hand.
#[test]
fn mark_good_moves_the_purge_protection_with_it() {
    let f = Fixture::new();
    std::fs::write(
        &f.config.boot.package_manager_conf,
        "multiversion.kernels = latest,latest-1,running,7.0.12-1.1\n",
    )
    .unwrap();
    let backend = MockBackend::with_packages(vec![
        pkg("kernel-default", "7.0.12-1.1", true),
        offered("kernel-default", "7.2.0-1.1", true),
        offered("kernel-default", "7.2.6-1.1", true),
    ]);
    let mut app = f.app(backend);

    app.mark_good("kernel", Some(Evr::parse("7.2.0-1.1")), now())
        .unwrap();
    app.mark_good("kernel", Some(Evr::parse("7.2.6-1.1")), now())
        .unwrap();

    let mv = sluice::boot::read_multiversion(&app.config.boot.package_manager_conf)
        .unwrap()
        .unwrap();
    assert_eq!(mv.render(), "latest,latest-1,running,7.0.12-1.1,7.2.6-1.1");
}

/// A within-series update must not silently move the rollback target.
#[test]
fn the_known_good_pin_does_not_follow_point_releases() {
    let f = Fixture::new();
    let mut app = f.app(f.tumbleweed_on_7_2());

    app.mark_good("kernel", Some(Evr::parse("7.2.0-1.1")), now())
        .unwrap();
    app.update(now()).unwrap();

    let state = app.state.component("kernel");
    assert_eq!(
        state.known_good.unwrap().to_string(),
        "7.2.0-1.1",
        "a regression inside the series must still leave somewhere to fall back to"
    );
}

/// The kernel family moves in one transaction, and firmware is not part of it.
#[test]
fn the_family_moves_as_one_unit() {
    let f = Fixture::new();
    // The handle is kept so the transaction can be inspected after the App has
    // taken ownership of the backend.
    let backend = std::rc::Rc::new(f.tumbleweed_on_7_2());
    let mut app = App::new(f.config.clone(), false)
        .unwrap()
        .with_backend(Box::new(std::rc::Rc::clone(&backend)));
    app.update(now()).unwrap();

    let installs = backend.installs.borrow().clone();
    let family_install = installs
        .iter()
        .find(|i| i.iter().any(|s| s.starts_with("kernel-default=")))
        .expect("the kernel family should have been installed");

    assert!(family_install.contains(&"kernel-default=7.2.6-1.1".to_string()));
    assert!(family_install.contains(&"kernel-devel=7.2.6-1.1".to_string()));
    assert!(
        !family_install.iter().any(|s| s.contains("firmware")),
        "date-versioned firmware is not part of the kernel family: {family_install:?}"
    );
}

/// migrate adopts the existing setup, and --undo restores it exactly.
#[test]
fn migrate_is_reversible() {
    let f = Fixture::new();
    let backend = f.tumbleweed_on_7_2();
    let mut r = Runner::new(false, None);
    let blanket = LockSpec::blanket("kernel-default");
    backend.add_lock(&mut r, &blanket).unwrap();

    let mut app = f.app(backend);

    let choice = BTreeMap::from([("kernel".to_string(), Evr::parse("7.0.12-1.1"))]);
    let report = app.migrate(&choice, now()).unwrap();
    assert_eq!(report.prior_locks, vec![blanket.clone()]);
    assert_eq!(
        app.state
            .component("kernel")
            .known_good
            .unwrap()
            .to_string(),
        "7.0.12-1.1"
    );

    let specs: Vec<String> = app
        .backend
        .locks(&mut app.runner)
        .unwrap()
        .iter()
        .map(LockSpec::spec_string)
        .collect();
    assert_eq!(
        specs,
        vec!["kernel-default >= 7.3", "kernel-devel >= 7.3"],
        "every family member is gated, or dup could move kernel-devel alone"
    );

    // Migrating twice must be refused rather than losing the original record.
    assert!(app.migrate(&BTreeMap::new(), now()).is_err());

    app.migrate_undo(now()).unwrap();
    let after: Vec<LockSpec> = app.backend.locks(&mut app.runner).unwrap();
    assert_eq!(
        after,
        vec![blanket],
        "undo must restore exactly what was there"
    );
    assert!(app.state.migration.is_none());

    let mv = sluice::boot::read_multiversion(&app.config.boot.package_manager_conf)
        .unwrap()
        .unwrap();
    assert_eq!(
        mv.render(),
        "latest,latest-1,running",
        "the prior setting must come back"
    );
}

/// `check` reports a newly available series and does not repeat itself.
#[test]
fn check_announces_a_gated_series_once() {
    let f = Fixture::new();
    let mut app = f.app(f.tumbleweed_on_7_3());

    let first = app.check(now(), false).unwrap();
    assert!(
        first.alerts.iter().any(|(_, m)| m.contains("7.3")),
        "got: {:?}",
        first.alerts
    );

    // A daily timer must not send the same announcement every day.
    let second = app.check(now() + Duration::days(1), false).unwrap();
    let repeated = second
        .alerts
        .iter()
        .filter(|(_, m)| m.contains("new 7.3 series"))
        .count();
    assert_eq!(repeated, 0, "got: {:?}", second.alerts);

    // --force overrides that.
    let forced = app.check(now() + Duration::days(2), true).unwrap();
    assert!(forced.alerts.iter().any(|(_, m)| m.contains("7.3")));
}

/// Once the repository stops carrying the held series, holding it no longer
/// means "keep getting fixes", and the user has to be told so.
#[test]
fn a_withdrawn_series_is_escalated_not_silently_tolerated() {
    let f = Fixture::new();
    let mut app = f.app(f.tumbleweed_on_7_3());

    let status = app.status(now()).unwrap();
    let kernel = &status.components[0];
    assert!(kernel.eval.series_withdrawn);

    let cfg = app.config.component("kernel").unwrap();
    let alerts = kernel.alerts(cfg);
    assert!(
        alerts.iter().any(|(_, m)| m.contains("no longer ship")),
        "got: {alerts:?}"
    );
}

/// `--dry-run` must describe without touching anything.
#[test]
fn dry_run_changes_nothing() {
    let f = Fixture::new();
    let backend = f.tumbleweed_on_7_2();
    let mut app = App::new(f.config.clone(), true)
        .unwrap()
        .with_backend(Box::new(backend));

    let report = app.update(now()).unwrap();
    assert_eq!(
        report.applied.len(),
        1,
        "a dry run still reports what it would do"
    );

    assert!(
        !f.config.paths.state_file().exists(),
        "a dry run must not persist state"
    );
    let mv = sluice::boot::read_multiversion(&app.config.boot.package_manager_conf)
        .unwrap()
        .unwrap();
    assert_eq!(
        mv.render(),
        "latest,latest-1,running",
        "zypp.conf must be untouched"
    );
}

/// Nothing to do is a first-class outcome, not an error.
#[test]
fn an_up_to_date_system_reports_cleanly() {
    let f = Fixture::new();
    let backend = MockBackend::with_packages(vec![pkg("kernel-default", "7.2.6-1.1", true)]);
    let mut app = f.app(backend);

    let report = app.update(now()).unwrap();
    assert!(report.applied.is_empty());
    assert!(report.gated.is_empty());

    let status = app.status(now()).unwrap();
    assert_eq!(status.components[0].eval.decision, Decision::UpToDate);
    assert_eq!(status.components[0].headline(), "up to date");
}

/// With `repo_changelog` on, the lineage view carries the distribution's own
/// changelog for the gated candidate.
#[test]
fn lineage_can_show_the_distribution_changelog() {
    let mut f = Fixture::new();
    f.config.lineage.repo_changelog = true;
    f.config.lineage.offline = false;
    // Upstream is unreachable: the view must still render, with a warning.
    f.config.lineage.releases_url = "http://127.0.0.1:1/releases.json".into();
    f.config.lineage.changelog_base = "http://127.0.0.1:1".into();

    let backend = f.tumbleweed_on_7_3();
    backend.changelogs.borrow_mut().insert(
        "kernel-default-7.3.3-1.1.x86_64".into(),
        "* Mon Sep 21 2026 kernel@example.com\n- Linux 7.3.3\n\n* Mon Sep 14 2026 kernel@example.com\n- Linux 7.3.2\n".into(),
    );
    let mut app = f.app(backend);

    let view = app.lineage("kernel", None, now()).unwrap();
    let candidate = view.candidate.expect("7.3 is gated");
    assert_eq!(candidate.series, "7.3");
    assert_eq!(candidate.repo_changelog.len(), 2);
    assert!(candidate.repo_changelog[0].contains("Linux 7.3.3"));
    assert!(
        !view.warnings.is_empty(),
        "an unreachable upstream is reported, not fatal"
    );
}
