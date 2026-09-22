//! Phase 2 of the specification: the GPU stack. Mesa is held by series, AMD
//! GPU firmware soaks, and kernel, Mesa and firmware can be promoted, marked
//! good and rolled back as one bundle.

use std::collections::BTreeMap;
use std::rc::Rc;

use chrono::{DateTime, Duration, Utc};
use sluice::app::App;
use sluice::backend::mock::MockBackend;
use sluice::backend::{LockSpec, Pkg, PkgStatus};
use sluice::config::{BundleConfig, ComponentConfig, Config, PackageSelector, Policy};
use sluice::policy::Decision;
use sluice::state::Lifecycle;

fn now() -> DateTime<Utc> {
    "2026-09-22T12:00:00Z".parse().unwrap()
}

/// A package as zypper reports it. Installed packages still carried by a
/// repository report that repository, as zypper does.
fn pkg(name: &str, evr: &str, installed: bool, source: Option<&str>) -> Pkg {
    Pkg {
        name: name.into(),
        evr: sluice::version::Evr::parse(evr),
        arch: "x86_64".into(),
        repo: "repo-oss".into(),
        status: if installed {
            PkgStatus::Installed
        } else {
            PkgStatus::Available
        },
        source: source.map(str::to_string),
    }
}

/// Mesa as it is really packaged: two sources at different releases, library
/// names outside `Mesa*`, and a `Mesa-demo` that is a different upstream.
fn mesa(version: &str, installed: bool) -> Vec<Pkg> {
    let (core, drivers) = (format!("{version}-2.1"), format!("{version}-2.2"));
    let src = |s| installed.then_some(s);
    vec![
        pkg("Mesa", &core, installed, src("Mesa")),
        pkg("libgbm1", &core, installed, src("Mesa")),
        pkg("Mesa-dri", &drivers, installed, src("Mesa-drivers")),
        pkg("libvulkan_radeon", &drivers, installed, src("Mesa-drivers")),
    ]
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
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
        config.boot.manager = sluice::config::BootManager::None;
        config.boot.efivars_dir = root.join("efivars");
        config.lineage.offline = true;
        config.notify.desktop = false;
        config.backend.refresh = false;
        std::fs::write(
            &config.boot.package_manager_conf,
            "multiversion.kernels = latest,latest-1,running\n",
        )
        .unwrap();

        config.components = BTreeMap::from([
            (
                "kernel".to_string(),
                ComponentConfig {
                    policy: Policy::HoldSeries,
                    anchor: Some("kernel-default".into()),
                    boot_entries: true,
                    ..Default::default()
                },
            ),
            (
                "mesa".to_string(),
                ComponentConfig {
                    policy: Policy::HoldSeries,
                    anchor: Some("Mesa".into()),
                    family_sources: vec!["Mesa".into(), "Mesa-drivers".into()],
                    ..Default::default()
                },
            ),
            (
                "amdgpu-firmware".to_string(),
                ComponentConfig {
                    policy: Policy::Soak,
                    soak_days: 7,
                    packages: PackageSelector::List(vec!["kernel-firmware-amdgpu".into()]),
                    anchor: Some("kernel-firmware-amdgpu".into()),
                    ..Default::default()
                },
            ),
        ]);
        config.bundles = BTreeMap::from([(
            "gpu".to_string(),
            BundleConfig {
                components: vec!["kernel".into(), "mesa".into(), "amdgpu-firmware".into()],
            },
        )]);
        Fixture { _dir: dir, config }
    }

    fn app(&self, backend: &Rc<MockBackend>) -> App {
        App::new(self.config.clone(), false)
            .unwrap()
            .with_backend(Box::new(Rc::clone(backend)))
    }
}

fn locks(app: &mut App) -> Vec<String> {
    let mut v: Vec<String> = app
        .backend
        .locks(&mut app.runner)
        .unwrap()
        .iter()
        .map(LockSpec::spec_string)
        .collect();
    v.sort();
    v
}

fn decision(app: &mut App, component: &str) -> Decision {
    app.evaluate(now())
        .unwrap()
        .into_iter()
        .find(|e| e.component == component)
        .unwrap()
        .decision
}

/// Mesa moves 26.2.2 → 26.2.4 on its own and holds 26.3 at the gate. Every
/// package built from its sources is gated, whatever its name; Mesa-demo,
/// which is a different upstream, is not.
#[test]
fn mesa_is_held_by_series_across_both_of_its_sources() {
    let f = Fixture::new();
    let mut pkgs = mesa("26.2.2", true);
    pkgs.extend(mesa("26.2.4", false));
    pkgs.extend(mesa("26.3.1", false));
    pkgs.push(pkg("Mesa-demo-x", "9.0.0-7.5", true, Some("Mesa-demo")));
    let backend = Rc::new(MockBackend::with_packages(pkgs));
    let mut app = f.app(&backend);

    let report = app.update(now()).unwrap();
    assert!(report
        .applied
        .iter()
        .any(|(c, v)| c == "mesa" && v.to_string() == "26.2.4-2.1"));
    assert!(report
        .gated
        .iter()
        .any(|(c, s, _)| c == "mesa" && s == "26.3"));

    let install = backend
        .installs
        .borrow()
        .iter()
        .find(|i| i.iter().any(|s| s.starts_with("Mesa=")))
        .cloned()
        .unwrap();
    assert!(install.contains(&"Mesa=26.2.4-2.1".to_string()));
    assert!(
        install.contains(&"Mesa-dri=26.2.4-2.2".to_string()),
        "the drivers source releases on its own schedule: {install:?}"
    );
    assert!(install.contains(&"libvulkan_radeon=26.2.4-2.2".to_string()));
    assert!(!install.iter().any(|s| s.contains("demo")));

    let gate = locks(&mut app);
    for name in ["Mesa", "Mesa-dri", "libgbm1", "libvulkan_radeon"] {
        assert!(
            gate.contains(&format!("{name} >= 26.3")),
            "{name} ungated: {gate:?}"
        );
    }
    assert!(!gate.iter().any(|l| l.contains("demo")));
}

/// Firmware waits out its soak window behind a lock, then applies.
#[test]
fn firmware_soaks_behind_a_lock_before_applying() {
    let f = Fixture::new();
    let backend = Rc::new(MockBackend::with_packages(vec![
        pkg("kernel-firmware-amdgpu", "20260829-1.1", true, None),
        pkg("kernel-firmware-amdgpu", "20260915-1.1", false, None),
    ]));
    let mut app = f.app(&backend);

    let report = app.update(now()).unwrap();
    assert_eq!(report.soaking.len(), 1);
    assert!(backend.installs.borrow().is_empty());
    assert_eq!(
        locks(&mut app),
        vec!["kernel-firmware-amdgpu > 20260829-1.1"],
        "without a lock, zypper dup would install it straight away"
    );

    let report = app.update(now() + Duration::days(7)).unwrap();
    assert!(report
        .applied
        .iter()
        .any(|(c, v)| c == "amdgpu-firmware" && v.to_string() == "20260915-1.1"));
    assert_eq!(
        locks(&mut app),
        vec!["kernel-firmware-amdgpu > 20260915-1.1"]
    );
}

/// A version newer than the soaking one restarts the clock.
#[test]
fn a_newer_firmware_restarts_the_soak() {
    let f = Fixture::new();
    let backend = Rc::new(MockBackend::with_packages(vec![
        pkg("kernel-firmware-amdgpu", "20260829-1.1", true, None),
        pkg("kernel-firmware-amdgpu", "20260915-1.1", false, None),
    ]));
    let mut app = f.app(&backend);
    app.update(now()).unwrap();

    backend
        .packages
        .borrow_mut()
        .push(pkg("kernel-firmware-amdgpu", "20260920-1.1", false, None));
    app.update(now() + Duration::days(5)).unwrap();
    assert!(matches!(
        decision(&mut app, "amdgpu-firmware"),
        Decision::Soaking { .. }
    ));
    // Seven days after the *first* candidate is not enough for the second.
    assert!(matches!(
        decision_at(&mut app, "amdgpu-firmware", now() + Duration::days(8)),
        Decision::Soaking { .. }
    ));
}

fn decision_at(app: &mut App, component: &str, at: DateTime<Utc>) -> Decision {
    app.evaluate(at)
        .unwrap()
        .into_iter()
        .find(|e| e.component == component)
        .unwrap()
        .decision
}

/// Rolling back a component with no series gate holds it, or the next update
/// would reinstall what was just rolled back.
#[test]
fn a_rolled_back_firmware_is_held_until_promoted() {
    let f = Fixture::new();
    let backend = Rc::new(MockBackend::with_packages(vec![
        pkg("kernel-firmware-amdgpu", "20260829-1.1", true, None),
        pkg("kernel-firmware-amdgpu", "20260915-1.1", false, None),
    ]));
    let mut app = f.app(&backend);
    app.mark_good("amdgpu-firmware", None, now()).unwrap();
    app.update(now() + Duration::days(7)).unwrap();

    // Firmware is not multiversion: the upgrade replaced the known-good one.
    {
        let mut pkgs = backend.packages.borrow_mut();
        pkgs.iter_mut()
            .filter(|p| p.evr.to_string() == "20260829-1.1")
            .for_each(|p| p.status = PkgStatus::Available);
    }

    let rolled = app
        .rollback("amdgpu-firmware", false, now() + Duration::days(8))
        .unwrap();
    assert_eq!(rolled.to.to_string(), "20260829-1.1");
    assert!(backend
        .installs
        .borrow()
        .iter()
        .any(|i| i == &vec!["kernel-firmware-amdgpu=20260829-1.1".to_string()]));

    assert_eq!(
        locks(&mut app),
        vec!["kernel-firmware-amdgpu > 20260829-1.1"]
    );
    // Pretend the downgrade happened, as zypper would have done it.
    {
        let mut pkgs = backend.packages.borrow_mut();
        for p in pkgs.iter_mut() {
            p.status = if p.evr.to_string() == "20260829-1.1" {
                PkgStatus::Installed
            } else {
                PkgStatus::Available
            };
        }
    }
    app.update(now() + Duration::days(30)).unwrap();
    assert!(matches!(
        decision_at(&mut app, "amdgpu-firmware", now() + Duration::days(30)),
        Decision::Held { .. }
    ));

    let promoted = app
        .promote("amdgpu-firmware", now() + Duration::days(31))
        .unwrap();
    assert_eq!(promoted.to.to_string(), "20260915-1.1");
    assert!(app.state.component("amdgpu-firmware").held_at.is_none());
}

/// kernel, Mesa and firmware cross together, in one transaction, and roll back
/// together.
#[test]
fn a_bundle_is_promoted_in_one_transaction_and_rolled_back_together() {
    let f = Fixture::new();
    let mut pkgs = vec![
        pkg("kernel-default", "7.2.6-1.1", true, None),
        pkg("kernel-default", "7.3.3-1.1", false, None),
        pkg("kernel-firmware-amdgpu", "20260829-1.1", true, None),
    ];
    pkgs.extend(mesa("26.2.4", true));
    pkgs.extend(mesa("26.3.1", false));
    let backend = Rc::new(MockBackend::with_packages(pkgs));
    let mut app = f.app(&backend);

    let marked = app.mark_good_bundle("gpu", now()).unwrap();
    assert_eq!(marked.len(), 3);
    assert!(marked.iter().all(|m| m.vaulted), "{marked:?}");
    backend.installs.borrow_mut().clear();

    let reports = app.promote_bundle("gpu", now()).unwrap();
    let installs = backend.installs.borrow().clone();
    assert_eq!(
        installs.len(),
        1,
        "one transaction for the whole stack: {installs:?}"
    );
    assert!(installs[0].contains(&"kernel-default=7.3.3-1.1".to_string()));
    assert!(installs[0].contains(&"Mesa=26.3.1-2.1".to_string()));

    let kept = reports
        .iter()
        .find(|r| r.component == "amdgpu-firmware")
        .unwrap();
    assert!(
        kept.packages.is_empty(),
        "firmware was not gated, so it did not move"
    );
    for c in ["kernel", "mesa", "amdgpu-firmware"] {
        assert_eq!(app.state.component(c).lifecycle, Lifecycle::Testing, "{c}");
    }

    let rolled = app.rollback_bundle("gpu", false, now()).unwrap();
    assert_eq!(rolled.len(), 3);
    for c in ["kernel", "mesa", "amdgpu-firmware"] {
        assert_eq!(
            app.state.component(c).lifecycle,
            Lifecycle::KnownGood,
            "{c}"
        );
    }
    assert_eq!(app.state.component("mesa").series.as_deref(), Some("26.2"));
}

#[test]
fn a_bundle_cannot_shadow_a_component() {
    let mut f = Fixture::new();
    f.config.bundles.insert(
        "kernel".into(),
        BundleConfig {
            components: vec!["mesa".into()],
        },
    );
    let text = toml::to_string(&f.config).unwrap();
    let path = f._dir.path().join("sluice.toml");
    std::fs::write(&path, text).unwrap();
    let err = Config::load(&path).unwrap_err().to_string();
    assert!(err.contains("same name as a component"), "{err}");
}
