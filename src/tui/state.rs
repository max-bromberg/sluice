//! Dashboard state: what is selected, what is loaded, what is being asked.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use ratatui::layout::Rect;

use super::timeline::{Source, TimelineView};
use super::worker::{Request, Response, Worker};
use crate::app::{App, Status};
use crate::config::{Config, LineageSource};
use crate::health::HealthReport;
use crate::privilege::{self, Escalator};
use crate::timeline::Relevance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Lineage,
    Health,
    Log,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Overview, Tab::Lineage, Tab::Health, Tab::Log];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Lineage => "Lineage",
            Tab::Health => "Health",
            Tab::Log => "Log",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// An action that needs privileges, waiting for the user to confirm it.
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub title: String,
    /// What running this will do, in plain words.
    pub consequence: String,
    /// The exact argv, shown before it runs — nothing is escalated invisibly.
    pub args: Vec<String>,
    pub command_line: String,
}

/// What the dashboard is currently showing over the main view.
#[derive(Debug, Clone)]
pub enum Modal {
    None,
    Confirm(PendingAction),
    Help,
    /// The result of the last escalated command.
    Output {
        title: String,
        body: String,
        ok: bool,
    },
}

/// A row in the component list: a component, or a bundle of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Component(String),
    Bundle(String),
}

impl Entry {
    pub fn name(&self) -> &str {
        match self {
            Entry::Component(n) | Entry::Bundle(n) => n,
        }
    }
}

pub struct Dashboard {
    pub app: App,
    pub status: Option<Status>,
    pub health: Option<HealthReport>,
    /// One timeline per component or bundle, built on first view and fed by
    /// the worker as upstream data arrives.
    pub timelines: BTreeMap<String, TimelineView>,
    pub worker: Worker,
    /// The info level carries over between timelines.
    pub info_level: u8,
    /// Where the component list and the tab bar were drawn, for the mouse.
    pub list_area: Rect,
    pub tabs_area: Rect,

    pub selected: usize,
    pub tab: Tab,
    pub modal: Modal,
    pub scroll: u16,

    pub log: Vec<String>,
    pub loading: Option<String>,
    pub last_refresh: Option<DateTime<Utc>>,
    pub escalator: Escalator,
    pub should_quit: bool,
}

impl Dashboard {
    pub fn new(config: Config, dry_run: bool) -> Result<Self> {
        let escalator = privilege::detect(&config.backend.escalate_with);
        let worker = Worker::spawn(
            config.lineage.clone(),
            config.paths.cache_dir.clone(),
            Relevance::detect(),
        );
        let app = App::new(config, dry_run)?;
        Ok(Dashboard {
            app,
            status: None,
            health: None,
            timelines: BTreeMap::new(),
            worker,
            info_level: 1,
            list_area: Rect::default(),
            tabs_area: Rect::default(),
            selected: 0,
            tab: Tab::Overview,
            modal: Modal::None,
            scroll: 0,
            log: Vec::new(),
            loading: None,
            last_refresh: None,
            escalator,
            should_quit: false,
        })
    }

    pub fn note(&mut self, msg: impl Into<String>) {
        let stamp = chrono::Local::now().format("%H:%M:%S");
        self.log.push(format!("{stamp}  {}", msg.into()));
        // The log pane is a session transcript, not an archive; the action log
        // on disk is the durable record.
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    pub fn refresh(&mut self) {
        let now = Utc::now();
        match self.app.status(now) {
            Ok(s) => {
                self.selected = self.selected.min(s.components.len().saturating_sub(1));
                self.status = Some(s);
                self.last_refresh = Some(now);
                self.note("refreshed");
            }
            Err(e) => self.note(format!("status failed: {e:#}")),
        }
        match crate::health::report(&self.app.config.health, &mut self.app.runner) {
            Ok(h) => self.health = Some(h),
            Err(e) => self.note(format!("health unavailable: {e:#}")),
        }
        // What is installed may have changed; timelines are rebuilt on view.
        self.timelines.clear();
        let n = self.entries().len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    /// Components, then bundles.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = self
            .status
            .as_ref()
            .map(|s| {
                s.components
                    .iter()
                    .map(|c| Entry::Component(c.eval.component.clone()))
                    .collect()
            })
            .unwrap_or_default();
        out.extend(self.app.config.bundles.keys().cloned().map(Entry::Bundle));
        out
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.entries().get(self.selected).cloned()
    }

    pub fn selected_component(&self) -> Option<&crate::app::ComponentStatus> {
        let Some(Entry::Component(name)) = self.selected_entry() else {
            return None;
        };
        self.status
            .as_ref()?
            .components
            .iter()
            .find(|c| c.eval.component == name)
    }

    pub fn selected_bundle(&self) -> Option<String> {
        match self.selected_entry() {
            Some(Entry::Bundle(b)) => Some(b),
            _ => None,
        }
    }

    pub fn component_status(&self, name: &str) -> Option<&crate::app::ComponentStatus> {
        self.status
            .as_ref()?
            .components
            .iter()
            .find(|c| c.eval.component == name)
    }

    pub fn selected_name(&self) -> Option<String> {
        self.selected_entry().map(|e| e.name().to_string())
    }

    pub fn select_next(&mut self) {
        let n = self.entries().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
            self.scroll = 0;
        }
    }

    pub fn select_prev(&mut self) {
        let n = self.entries().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
            self.scroll = 0;
        }
    }

    /// The timeline for the current selection, building it if needed. What
    /// this machine has is read now; upstream history arrives from the worker.
    pub fn ensure_timeline(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let name = entry.name().to_string();
        if self.timelines.contains_key(&name) {
            return;
        }
        let members: Vec<String> = match &entry {
            Entry::Component(c) => vec![c.clone()],
            Entry::Bundle(b) => self
                .app
                .config
                .bundles
                .get(b)
                .map(|b| b.components.clone())
                .unwrap_or_default(),
        };
        // Draw the empty canvas first; reading this machine takes a moment,
        // and happens on the next pass through the loop.
        let sources = members
            .iter()
            .filter_map(|m| {
                let cfg = self.app.config.component(m).ok()?;
                Some(Source {
                    component: m.clone(),
                    upstream: None,
                    machine: Default::default(),
                    shapes: BTreeMap::new(),
                    has_shapes: matches!(
                        cfg.lineage,
                        LineageSource::LinuxStable | LineageSource::Mesa
                    ),
                    boots: cfg.boot_entries,
                })
            })
            .collect();
        let mut view = TimelineView::new(name.clone(), sources, self.info_level);
        view.reading_machine = true;
        self.timelines.insert(name, view);
    }

    /// The slow half of building a timeline: what this machine has, then the
    /// request for upstream history.
    pub fn read_machine(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let pending: Vec<String> = match self.timelines.get(&name) {
            Some(tl) if tl.reading_machine => {
                tl.sources.iter().map(|s| s.component.clone()).collect()
            }
            _ => return,
        };
        for m in pending {
            let eval = self.component_status(&m).map(|c| c.eval.clone());
            let now = Utc::now();
            let eval = match eval {
                Some(e) => e,
                None => match self
                    .app
                    .evaluate(now)
                    .ok()
                    .and_then(|es| es.into_iter().find(|e| e.component == m))
                {
                    Some(e) => e,
                    None => continue,
                },
            };
            let machine = match self.app.machine_for(&eval, self.health.as_ref()) {
                Ok(machine) => machine,
                Err(e) => {
                    self.note(format!("{m}: could not read what is installed: {e:#}"));
                    Default::default()
                }
            };
            let keep = self
                .app
                .timeline_keep_for(&eval, &machine)
                .unwrap_or_default();
            if let Ok(cfg) = self.app.config.component(&m).cloned() {
                let _ = self.worker.requests.send(Request::Upstream {
                    component: m.clone(),
                    cfg,
                    keep,
                });
            }
            if let Some(tl) = self.timelines.get_mut(&name) {
                tl.set_machine(&m, machine);
            }
        }
        if let Some(tl) = self.timelines.get_mut(&name) {
            tl.reading_machine = false;
        }
    }

    pub fn active_timeline(&mut self) -> Option<&mut TimelineView> {
        let name = self.selected_name()?;
        self.timelines.get_mut(&name)
    }

    /// Take in whatever the worker has finished, and ask it for what the
    /// visible timeline needs next.
    pub fn pump(&mut self) {
        while let Ok(response) = self.worker.responses.try_recv() {
            match response {
                Response::Upstream {
                    component,
                    upstream,
                } => {
                    for w in &upstream.warnings {
                        self.note(format!("{component}: {w}"));
                    }
                    for tl in self.timelines.values_mut() {
                        tl.set_upstream(&component, upstream.clone());
                    }
                }
                Response::Shape {
                    component,
                    version,
                    shape,
                } => {
                    for tl in self.timelines.values_mut() {
                        tl.set_shape(&component, &version, shape.clone());
                    }
                }
            }
        }
        if self.tab != Tab::Lineage {
            return;
        }
        let requests: Vec<(String, String)> = self
            .active_timeline()
            .map(|tl| tl.wanted_shapes())
            .unwrap_or_default();
        for (component, version) in requests {
            let Ok(cfg) = self.app.config.component(&component).cloned() else {
                continue;
            };
            if let Some(tl) = self.active_timeline() {
                tl.mark_loading(&component, &version);
            }
            let _ = self.worker.requests.send(Request::Shape {
                component,
                cfg,
                version,
            });
        }
    }

    /// Build the confirmation for a mutating action on the current selection.
    pub fn plan(&self, verb: Verb) -> Option<PendingAction> {
        if let Some(bundle) = self.selected_bundle() {
            return self.plan_bundle(verb, &bundle);
        }
        let component = self.selected_name()?;
        let c = self.selected_component()?;

        let (args, consequence) = match verb {
            Verb::Update => (
                vec!["update".to_string()],
                "Refresh, apply every update your policies allow, and report anything gated. \
                 Nothing crosses a series boundary."
                    .to_string(),
            ),
            Verb::Promote => {
                let consequence = match (c.eval.gated_series(), &c.eval.decision, &c.eval.held_at) {
                    (Some((series, target)), _, _) => format!(
                        "Cross the series gate: install {target} and move {component} onto the {series} series. \
                         This is a feature upgrade. It will be marked as testing, not known-good."
                    ),
                    (None, crate::policy::Decision::Held { target }, Some(held)) => format!(
                        "Release the rollback hold on {component} at {held} and install {target}. \
                         It will be marked as testing, not known-good."
                    ),
                    _ => return None,
                };
                (
                    vec!["promote".into(), component.clone(), "--yes".into()],
                    consequence,
                )
            }
            Verb::MarkGood => {
                let v = c.eval.installed.as_ref()?;
                (
                    vec!["mark-good".into(), component.clone()],
                    format!(
                        "Record {v} as known-good: vault its RPMs so it can be reinstalled after the \
                         repository drops it, and pin it so purge-kernels cannot remove it."
                    ),
                )
            }
            Verb::Rollback => {
                let kg = c.known_good.as_ref()?;
                (
                    vec!["rollback".into(), component.clone()],
                    format!(
                        "Make {kg} the default boot entry again, reinstalling it from the vault if needed. \
                         The version under test is left installed."
                    ),
                )
            }
        };

        self.action(verb, &component, args, consequence)
    }

    fn action(
        &self,
        verb: Verb,
        name: &str,
        args: Vec<String>,
        consequence: String,
    ) -> Option<PendingAction> {
        let exe = privilege::current_exe().ok()?;
        let config = self.app.config.source.clone();
        let cmd = privilege::escalated(self.escalator, &exe, config.as_deref(), &args);
        Some(PendingAction {
            title: format!("{} {name}", verb.label()),
            consequence,
            args,
            command_line: cmd.display(),
        })
    }

    fn plan_bundle(&self, verb: Verb, bundle: &str) -> Option<PendingAction> {
        let members = self.app.config.bundles.get(bundle)?.components.clone();
        let list = members.join(", ");
        let (args, consequence) = match verb {
            Verb::Update => (
                vec!["update".to_string()],
                "Refresh, apply every update your policies allow, and report anything gated.".to_string(),
            ),
            Verb::Promote => {
                let moving: Vec<String> = members
                    .iter()
                    .filter_map(|m| {
                        let c = self.component_status(m)?;
                        c.eval.gated_series().map(|(s, v)| format!("{m} → {v} ({s})"))
                    })
                    .collect();
                if moving.is_empty() {
                    return None;
                }
                (
                    vec!["promote".into(), bundle.to_string(), "--yes".into()],
                    format!(
                        "Promote the {bundle} bundle in one transaction: {}. Every member is marked as testing.",
                        moving.join(", ")
                    ),
                )
            }
            Verb::MarkGood => (
                vec!["mark-good".into(), bundle.to_string()],
                format!("Record the installed versions of {list} as known-good together: vault and pin each."),
            ),
            Verb::Rollback => (
                vec!["rollback".into(), bundle.to_string()],
                format!("Roll {list} back to their known-good versions together."),
            ),
        };
        self.action(verb, bundle, args, consequence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Update,
    Promote,
    MarkGood,
    Rollback,
}

impl Verb {
    pub fn label(self) -> &'static str {
        match self {
            Verb::Update => "update",
            Verb::Promote => "promote",
            Verb::MarkGood => "mark good",
            Verb::Rollback => "roll back",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_cycle_in_both_directions() {
        assert_eq!(Tab::Overview.next(), Tab::Lineage);
        assert_eq!(Tab::Log.next(), Tab::Overview);
        assert_eq!(Tab::Overview.prev(), Tab::Log);
        assert_eq!(Tab::Lineage.index(), 1);
    }

    #[test]
    fn verb_labels_read_as_commands() {
        assert_eq!(Verb::MarkGood.label(), "mark good");
        assert_eq!(Verb::Promote.label(), "promote");
    }
}
