//! The gating decision engine.
//!
//! This module is deliberately pure: it takes the installed set, the available
//! set and the recorded state, and returns a decision. Nothing here touches the
//! system, which is what makes the interesting cases testable.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;

use crate::backend::Pkg;
use crate::config::{ComponentConfig, Config, PackageSelector, Policy};
use crate::state::ComponentState;
use crate::version::{series_regex, Evr};

/// What sluice intends to do about a component, this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing newer is offered.
    UpToDate,
    /// A same-series update the policy allows.
    Apply { target: Evr },
    /// A series change, held back for an explicit promotion.
    Gated { series: String, target: Evr },
    /// `soak`: the candidate is new enough that it has not settled yet.
    Soaking {
        target: Evr,
        ready_at: DateTime<Utc>,
    },
    /// `hold`: never automatic.
    Held { target: Evr },
}

impl Decision {
    pub fn is_actionable(&self) -> bool {
        matches!(self, Decision::Apply { .. })
    }

    pub fn needs_attention(&self) -> bool {
        matches!(self, Decision::Gated { .. } | Decision::Held { .. })
    }
}

/// Everything `status`, `update` and the TUI need to render one component.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub component: String,
    pub policy: Policy,
    pub installed: Option<Evr>,
    pub series: Option<String>,
    /// The newest version the repositories offer, whatever its series.
    pub candidate: Option<Evr>,
    /// The newest version offered *within the installed series*.
    pub series_candidate: Option<Evr>,
    pub decision: Decision,
    /// The exact packages to install if the decision is actionable.
    pub targets: Vec<Pkg>,
    /// The installed packages that make up this component.
    pub family: Vec<Pkg>,
    /// True when the repositories no longer carry the installed series at all,
    /// which on a rolling distribution means the hold has stopped receiving
    /// fixes rather than merely deferring a feature jump.
    pub series_withdrawn: bool,
    /// A rollback hold in force; see `ComponentState::held_at`.
    pub held_at: Option<Evr>,
    /// A newer series waiting at the gate, as `(series, version)`. Tracked
    /// apart from the decision because both can be true at once: 7.2.7 is
    /// applied while 7.3.3 waits, and you should hear about 7.3 now, not after.
    pub gated: Option<(String, Evr)>,
}

impl Evaluation {
    /// The gated series, if any, whatever the decision is.
    pub fn gated_series(&self) -> Option<(&str, &Evr)> {
        self.gated.as_ref().map(|(s, v)| (s.as_str(), v))
    }
}

impl Evaluation {
    pub fn family_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.family.iter().map(|p| p.name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Resolve which installed packages make up a component at `version`.
///
/// For `packages = "auto"` the rule is: the anchor package, plus every
/// installed package inside `family_glob` carrying the *same* EVR as the
/// anchor. Sharing a version is what actually distinguishes `kernel-default-devel`
/// (part of the family) from `kernel-firmware-amdgpu` (date-versioned, not part
/// of it), and it needs no hardcoded exclusion list.
///
/// `version` defaults to the newest installed anchor. It is explicit because
/// several versions of a multiversion package can be installed at once, and
/// the family that matters is the one at the version being acted on.
pub fn resolve_family(cfg: &ComponentConfig, installed: &[Pkg], version: Option<&Evr>) -> Vec<Pkg> {
    match &cfg.packages {
        PackageSelector::List(names) => installed
            .iter()
            .filter(|p| names.contains(&p.name))
            .cloned()
            .collect(),
        PackageSelector::Pattern(p) if p != "auto" => installed.to_vec(),
        PackageSelector::Pattern(_) => {
            let Some(anchor_name) = &cfg.anchor else {
                return Vec::new();
            };
            let anchor_evr = match version {
                Some(v) => v.clone(),
                None => match newest(installed.iter().filter(|p| &p.name == anchor_name)) {
                    Some(p) => p.evr.clone(),
                    None => return Vec::new(),
                },
            };

            let by_source = !cfg.family_sources.is_empty();
            let mut family: Vec<Pkg> = installed
                .iter()
                .filter(|p| same_build(cfg, &p.evr, &anchor_evr))
                .filter(|p| {
                    !by_source
                        || p.source
                            .as_ref()
                            .is_some_and(|s| cfg.family_sources.contains(s))
                })
                .cloned()
                .collect();

            if cfg.include_kmps {
                for p in installed {
                    if is_kmp(&p.name)
                        && kmp_built_for(&p.evr, &anchor_evr)
                        && !family.iter().any(|f| f.name == p.name)
                    {
                        family.push(p.clone());
                    }
                }
            }

            family.sort_by(|a, b| a.name.cmp(&b.name));
            family
        }
    }
}

/// Whether two versions are the same build for family purposes. By default
/// that is the exact EVR. A source-grouped family only shares the upstream
/// version, since its source packages are released independently.
pub fn same_build(cfg: &ComponentConfig, a: &Evr, b: &Evr) -> bool {
    if cfg.family_sources.is_empty() {
        a == b
    } else {
        a.epoch == b.epoch && a.version == b.version
    }
}

/// Kernel module packages, which are versioned by their own upstream and bound
/// to a kernel only through the marker in their version.
pub fn is_kmp(name: &str) -> bool {
    name.contains("-kmp-")
}

/// Whether a KMP version was built against `kernel`. openSUSE encodes that as
/// `_k<kernel version>_<kernel release major>` in the KMP's version, e.g.
/// `7.1.4_k6.12.8_1`. Some builds flatten the dots to `_`.
pub fn kmp_built_for(kmp: &Evr, kernel: &Evr) -> bool {
    let rel_major = kernel.release.split('.').next().unwrap_or("");
    let marker = format!("_k{}_{}", kernel.version, rel_major);
    let flat = marker.replace('.', "_");
    contains_marker(&kmp.version, &marker) || contains_marker(&kmp.version, &flat)
}

/// `marker` occurs in `s` and is not followed by another digit, so that
/// `_k7.2.0_1` does not match a KMP built for `7.2.0-10`.
fn contains_marker(s: &str, marker: &str) -> bool {
    s.match_indices(marker)
        .any(|(i, m)| !s[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

fn newest<'a>(it: impl Iterator<Item = &'a Pkg>) -> Option<&'a Pkg> {
    it.max_by(|a, b| a.evr.cmp(&b.evr))
}

/// Evaluate one component.
///
/// `installed` and `available` are the backend's view of the packages matching
/// this component; `now` is injected so soak windows are testable.
pub fn evaluate(
    name: &str,
    cfg: &ComponentConfig,
    state: &mut ComponentState,
    installed: &[Pkg],
    available: &[Pkg],
    now: DateTime<Utc>,
) -> Result<Evaluation> {
    let re = series_regex(&cfg.series_regex)?;

    let anchor_name = cfg.anchor.clone().or_else(|| {
        resolve_family(cfg, installed, None)
            .first()
            .map(|p| p.name.clone())
    });
    let anchors: Vec<&Pkg> = match &anchor_name {
        Some(n) => installed.iter().filter(|p| &p.name == n).collect(),
        None => Vec::new(),
    };

    // The series being held: recorded in state if sluice has been told, else
    // whatever the newest installed version belongs to.
    let newest_installed = newest(anchors.iter().copied());
    let series = state
        .series
        .clone()
        .or_else(|| newest_installed.and_then(|p| p.evr.series(&re)));

    // "Installed" means the newest installed version *inside that series*. A
    // newer version outside it — a tested kernel kept after a rollback — is
    // installed but not what this component is on.
    let installed_evr = series
        .as_ref()
        .and_then(|s| {
            newest(
                anchors
                    .iter()
                    .copied()
                    .filter(|p| p.evr.series(&re).as_ref() == Some(s)),
            )
        })
        .or(newest_installed)
        .map(|p| p.evr.clone());

    let family = resolve_family(cfg, installed, installed_evr.as_ref());

    // Candidates are versions strictly newer than what the component is on.
    // An installed version outside the series still counts: after a rollback
    // it is exactly what `promote` would move back to.
    let mut candidates: Vec<&Pkg> = match &anchor_name {
        Some(n) => available
            .iter()
            .filter(|p| &p.name == n)
            .filter(|p| installed_evr.as_ref().is_none_or(|i| &p.evr > i))
            .collect(),
        None => Vec::new(),
    };
    candidates.sort_by(|a, b| a.evr.cmp(&b.evr));
    candidates.dedup_by(|a, b| a.evr == b.evr);

    for c in &candidates {
        state.observe(&c.evr, now);
    }
    state.retain_seen(&candidates.iter().map(|p| p.evr.clone()).collect::<Vec<_>>());

    let candidate = candidates.last().map(|p| p.evr.clone());
    let series_candidate = series.as_ref().and_then(|s| {
        candidates
            .iter()
            .rfind(|p| p.evr.series(&re).as_ref() == Some(s))
            .map(|p| p.evr.clone())
    });

    // The installed series has been withdrawn when the repositories offer
    // something newer, but nothing at all in the series we are sitting on.
    let series_withdrawn = match (&series, &anchor_name) {
        (Some(s), Some(n)) => {
            let offers_series = available
                .iter()
                .any(|p| &p.name == n && p.offered() && p.evr.series(&re).as_ref() == Some(s));
            !offers_series && candidate.is_some()
        }
        _ => false,
    };

    let held_at = state
        .held_at
        .clone()
        .filter(|_| cfg.policy != Policy::HoldSeries);
    let decision = match (&held_at, &candidate) {
        // A rolled-back component waits for an explicit promote, whatever its
        // policy would otherwise do.
        (Some(_), Some(c)) => Decision::Held { target: c.clone() },
        _ => decide(
            cfg,
            state,
            &series,
            &re,
            candidate.as_ref(),
            series_candidate.as_ref(),
            now,
        ),
    };

    let gated = match (&decision, cfg.policy) {
        (Decision::Gated { series, target }, _) => Some((series.clone(), target.clone())),
        (_, Policy::HoldSeries) => {
            candidate
                .as_ref()
                .filter(|c| c.series(&re) != series)
                .map(|c| {
                    (
                        c.series(&re).unwrap_or_else(|| c.version.clone()),
                        c.clone(),
                    )
                })
        }
        _ => None,
    };

    let targets = match &decision {
        Decision::Apply { target } => family_at(cfg, &family, available, target),
        _ => Vec::new(),
    };

    Ok(Evaluation {
        component: name.to_string(),
        policy: cfg.policy,
        installed: installed_evr,
        series,
        candidate,
        series_candidate,
        decision,
        targets,
        family,
        series_withdrawn,
        held_at,
        gated,
    })
}

fn decide(
    cfg: &ComponentConfig,
    state: &ComponentState,
    series: &Option<String>,
    re: &Regex,
    candidate: Option<&Evr>,
    series_candidate: Option<&Evr>,
    now: DateTime<Utc>,
) -> Decision {
    let Some(candidate) = candidate else {
        return Decision::UpToDate;
    };

    match cfg.policy {
        Policy::Follow => Decision::Apply {
            target: candidate.clone(),
        },

        Policy::Hold => Decision::Held {
            target: candidate.clone(),
        },

        Policy::HoldSeries => {
            // A same-series update always flows, even while `testing`: holding a
            // series is about refusing feature jumps, not about refusing fixes.
            if let Some(sc) = series_candidate {
                return Decision::Apply { target: sc.clone() };
            }
            let candidate_series = candidate
                .series(re)
                .or_else(|| series.clone())
                .unwrap_or_else(|| candidate.version.clone());
            Decision::Gated {
                series: candidate_series,
                target: candidate.clone(),
            }
        }

        Policy::Soak => {
            let first_seen = state
                .first_seen
                .get(&candidate.to_string())
                .copied()
                .unwrap_or(now);
            let ready_at = first_seen + Duration::days(i64::from(cfg.soak_days));
            if now >= ready_at {
                Decision::Apply {
                    target: candidate.clone(),
                }
            } else {
                Decision::Soaking {
                    target: candidate.clone(),
                    ready_at,
                }
            }
        }
    }
}

/// The whole family at `target`'s version, so it moves as one transaction.
///
/// A member with no build at the target version is dropped rather than pinned
/// at its old version, since installing a partial family is worse than letting
/// the resolver pull what it needs.
fn family_at(cfg: &ComponentConfig, family: &[Pkg], available: &[Pkg], target: &Evr) -> Vec<Pkg> {
    let mut out = Vec::new();
    for member in family {
        if let Some(p) = available
            .iter()
            .filter(|p| p.name == member.name && same_build(cfg, &p.evr, target))
            .max_by(|a, b| a.evr.cmp(&b.evr))
        {
            out.push(p.clone());
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Looks up a component's `(installed, available)` packages.
pub type PackageLookup<'a> = dyn FnMut(&str, &ComponentConfig) -> Result<(Vec<Pkg>, Vec<Pkg>)> + 'a;

/// Evaluate every configured component.
pub fn evaluate_all(
    config: &Config,
    states: &mut crate::state::State,
    lookup: &mut PackageLookup<'_>,
    now: DateTime<Utc>,
) -> Result<Vec<Evaluation>> {
    let mut out = Vec::new();
    for (name, cfg) in &config.components {
        let (installed, available) = lookup(name, cfg)?;
        let state = states.component_mut(name);
        out.push(evaluate(name, cfg, state, &installed, &available, now)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PkgStatus;

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

    fn kernel_cfg(policy: Policy) -> ComponentConfig {
        ComponentConfig {
            policy,
            anchor: Some("kernel-default".into()),
            ..Default::default()
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-09-22T12:00:00Z".parse().unwrap()
    }

    /// The motivating situation: installed 7.2.0,
    /// repo has 7.2.6. That is a fix release and must flow without asking.
    #[test]
    fn same_series_update_is_applied() {
        let installed = vec![
            pkg("kernel-default", "7.2.0-1.1", true),
            pkg("kernel-devel", "7.2.0-1.1", true),
            pkg("kernel-firmware-amdgpu", "20260829-1.1", true),
        ];
        let available = vec![
            pkg("kernel-default", "7.2.6-1.1", false),
            pkg("kernel-devel", "7.2.6-1.1", false),
        ];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();

        assert_eq!(
            e.decision,
            Decision::Apply {
                target: Evr::parse("7.2.6-1.1")
            }
        );
        assert_eq!(e.series.as_deref(), Some("7.2"));
        assert!(!e.series_withdrawn);

        // Date-versioned firmware must not be swept into the kernel family.
        assert_eq!(e.family_names(), vec!["kernel-default", "kernel-devel"]);
        assert_eq!(e.targets.len(), 2, "the family moves as one unit");
    }

    #[test]
    fn a_new_series_is_gated_not_applied() {
        let installed = vec![pkg("kernel-default", "7.2.6-1.1", true)];
        let available = vec![pkg("kernel-default", "7.3.1-1.1", false)];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();

        assert_eq!(
            e.decision,
            Decision::Gated {
                series: "7.3".into(),
                target: Evr::parse("7.3.1-1.1")
            }
        );
        assert!(
            e.targets.is_empty(),
            "a gated component must install nothing"
        );
    }

    /// Both a remaining fix release and a new series are offered: take the fix,
    /// gate the jump. Preferring the newest candidate here would silently
    /// promote across a feature boundary.
    #[test]
    fn a_remaining_fix_release_wins_over_a_new_series() {
        let installed = vec![pkg("kernel-default", "7.2.4-1.1", true)];
        let available = vec![
            pkg("kernel-default", "7.2.6-1.1", false),
            pkg("kernel-default", "7.3.1-1.1", false),
        ];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();
        assert_eq!(
            e.decision,
            Decision::Apply {
                target: Evr::parse("7.2.6-1.1")
            }
        );
        assert_eq!(e.candidate.unwrap().to_string(), "7.3.1-1.1");
    }

    /// The rolling-distribution trap from the spec: once the repo moves on,
    /// holding a series stops meaning "keep getting fixes".
    #[test]
    fn withdrawn_series_is_detected() {
        let installed = vec![pkg("kernel-default", "7.2.6-1.1", true)];
        let available = vec![pkg("kernel-default", "7.3.1-1.1", false)];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();
        assert!(e.series_withdrawn);
    }

    #[test]
    fn up_to_date_when_nothing_newer_is_offered() {
        let installed = vec![pkg("kernel-default", "7.2.6-1.1", true)];
        let available = vec![pkg("kernel-default", "7.2.0-1.1", false)];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();
        assert_eq!(e.decision, Decision::UpToDate);
        assert!(!e.series_withdrawn, "an older leftover is not a withdrawal");
    }

    #[test]
    fn follow_takes_the_newest_regardless_of_series() {
        let installed = vec![pkg("Mesa", "26.2.2-2.2", true)];
        let available = vec![pkg("Mesa", "26.3.0-1.1", false)];
        let cfg = ComponentConfig {
            policy: Policy::Follow,
            anchor: Some("Mesa".into()),
            packages: PackageSelector::Pattern("Mesa*".into()),
            ..Default::default()
        };
        let mut state = ComponentState::default();
        let e = evaluate("mesa", &cfg, &mut state, &installed, &available, now()).unwrap();
        assert_eq!(
            e.decision,
            Decision::Apply {
                target: Evr::parse("26.3.0-1.1")
            }
        );
    }

    #[test]
    fn hold_never_applies() {
        let installed = vec![pkg("kernel-default", "7.2.0-1.1", true)];
        let available = vec![pkg("kernel-default", "7.2.6-1.1", false)];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::Hold),
            &mut state,
            &installed,
            &available,
            now(),
        )
        .unwrap();
        assert_eq!(
            e.decision,
            Decision::Held {
                target: Evr::parse("7.2.6-1.1")
            }
        );
    }

    #[test]
    fn soak_waits_for_the_window_then_applies() {
        let installed = vec![pkg("kernel-firmware-amdgpu", "20260829-1.1", true)];
        let available = vec![pkg("kernel-firmware-amdgpu", "20260915-1.1", false)];
        let cfg = ComponentConfig {
            policy: Policy::Soak,
            soak_days: 7,
            anchor: Some("kernel-firmware-amdgpu".into()),
            packages: PackageSelector::List(vec!["kernel-firmware-amdgpu".into()]),
            ..Default::default()
        };
        let mut state = ComponentState::default();

        let seen = now();
        let e = evaluate("fw", &cfg, &mut state, &installed, &available, seen).unwrap();
        let Decision::Soaking { ready_at, .. } = e.decision else {
            panic!("expected Soaking, got {:?}", e.decision);
        };
        assert_eq!(ready_at, seen + Duration::days(7));

        // Six days on, still soaking; eight days on, it applies.
        let e = evaluate(
            "fw",
            &cfg,
            &mut state,
            &installed,
            &available,
            seen + Duration::days(6),
        )
        .unwrap();
        assert!(matches!(e.decision, Decision::Soaking { .. }));
        let e = evaluate(
            "fw",
            &cfg,
            &mut state,
            &installed,
            &available,
            seen + Duration::days(8),
        )
        .unwrap();
        assert!(matches!(e.decision, Decision::Apply { .. }));
    }

    #[test]
    fn kmps_built_against_the_anchor_join_the_family() {
        let installed = vec![
            pkg("kernel-default", "7.2.0-1.1", true),
            pkg("vboxhost-kmp-default", "7.1.4_k7.2.0_1-1.1", true),
            pkg("flat-kmp-default", "2.0_k7_2_0_1-1.1", true),
            pkg("some-kmp-default", "3.0_k7.0.12_1-1.1", true),
            pkg("near-miss-kmp-default", "1.0_k7.2.0_10-1.1", true),
        ];
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &installed,
            &[],
            now(),
        )
        .unwrap();
        let names = e.family_names();
        assert!(names.contains(&"vboxhost-kmp-default".to_string()));
        assert!(names.contains(&"flat-kmp-default".to_string()));
        assert!(!names.contains(&"near-miss-kmp-default".to_string()));
        assert!(
            !names.contains(&"some-kmp-default".to_string()),
            "a KMP built against a different kernel is not in this family"
        );
    }

    #[test]
    fn missing_component_packages_yield_no_decision_rather_than_an_error() {
        let mut state = ComponentState::default();
        let e = evaluate(
            "kernel",
            &kernel_cfg(Policy::HoldSeries),
            &mut state,
            &[],
            &[],
            now(),
        )
        .unwrap();
        assert_eq!(e.decision, Decision::UpToDate);
        assert!(e.installed.is_none());
    }
}
