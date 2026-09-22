//! Dashboard state: what is selected, what is loaded, what is being asked.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::app::{App, Status};
use crate::config::Config;
use crate::health::HealthReport;
use crate::lineage::LineageView;
use crate::privilege::{self, Escalator};

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

pub struct Dashboard {
    pub app: App,
    pub status: Option<Status>,
    pub health: Option<HealthReport>,
    pub lineage: Option<LineageView>,
    /// The component the lineage view was loaded for, so a selection change
    /// invalidates it rather than showing another component's data.
    pub lineage_for: Option<String>,

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
        let app = App::new(config, dry_run)?;
        Ok(Dashboard {
            app,
            status: None,
            health: None,
            lineage: None,
            lineage_for: None,
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
        // The lineage belongs to whichever component was selected before.
        self.lineage = None;
        self.lineage_for = None;
    }

    pub fn component_names(&self) -> Vec<String> {
        self.status
            .as_ref()
            .map(|s| {
                s.components
                    .iter()
                    .map(|c| c.eval.component.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn selected_component(&self) -> Option<&crate::app::ComponentStatus> {
        self.status.as_ref()?.components.get(self.selected)
    }

    pub fn selected_name(&self) -> Option<String> {
        self.selected_component().map(|c| c.eval.component.clone())
    }

    pub fn select_next(&mut self) {
        let n = self.component_names().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
            self.scroll = 0;
        }
    }

    pub fn select_prev(&mut self) {
        let n = self.component_names().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
            self.scroll = 0;
        }
    }

    /// Load the lineage for the current selection, if it is not already loaded.
    pub fn ensure_lineage(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        if self.lineage_for.as_deref() == Some(name.as_str()) {
            return;
        }
        self.loading = Some(format!("fetching lineage for {name}…"));
        match self.app.lineage(&name, None, Utc::now()) {
            Ok(v) => {
                self.note(format!("lineage for {name}: {}", v.freshness_note()));
                self.lineage = Some(v);
                self.lineage_for = Some(name);
            }
            Err(e) => self.note(format!("lineage failed: {e:#}")),
        }
        self.loading = None;
    }

    /// Build the confirmation for a mutating action on the current selection.
    pub fn plan(&self, verb: Verb) -> Option<PendingAction> {
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

        let exe = privilege::current_exe().ok()?;
        let config = self.app.config.source.clone();
        let cmd = privilege::escalated(self.escalator, &exe, config.as_deref(), &args);

        Some(PendingAction {
            title: format!("{} {component}", verb.label()),
            consequence,
            args,
            command_line: cmd.display(),
        })
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
