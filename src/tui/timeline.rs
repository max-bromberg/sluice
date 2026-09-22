//! The interactive release timeline.
//!
//! A canvas of time: one lane per series, releases as markers along it, this
//! machine's versions picked out on top, and the releases still being
//! developed drawn as ghosts where their cadence says they will land. It can
//! be scrubbed release by release, dragged, and zoomed from days to years.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::{Duration, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::canvas::*;
use crate::timeline::{
    LaneState, Machine, Projection, ProjectionKind, Release, ReleaseKind, Shape, Tier, Upstream,
    VersionMarks,
};

// The dashboard palette, plus the timeline's own accents.
pub const MAX_INFO: u8 = 3;

/// Where and when a lane is being drawn.
#[derive(Clone, Copy)]
struct Band {
    y: u16,
    h: u16,
    reveal_col: u16,
    elapsed: f64,
}
const MIN_SCALE: f64 = 0.15;
const MAX_SCALE: f64 = 90.0;
pub enum ShapeState {
    Loading,
    Ready(Shape),
    Unavailable,
}

/// One component's data on the canvas.
pub struct Source {
    pub component: String,
    pub upstream: Option<Upstream>,
    pub machine: Machine,
    pub shapes: BTreeMap<String, ShapeState>,
    /// The lineage source can say what a release changed.
    pub has_shapes: bool,
    pub boots: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Lane { source: usize, lane: usize },
    Machine { source: usize },
}

/// Something on the canvas that can be selected.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemRef {
    Release {
        source: usize,
        lane: usize,
        index: usize,
    },
    Projection {
        source: usize,
        lane: usize,
        index: usize,
    },
    Install {
        source: usize,
        version: String,
        date: NaiveDate,
    },
    Removal {
        source: usize,
        version: String,
        date: NaiveDate,
    },
    Boot {
        source: usize,
        index: usize,
    },
}

#[derive(Debug, Clone, Copy)]
enum Hit {
    ZoomIn,
    ZoomOut,
    Less,
    More,
    Fit,
    Today,
}

pub struct TimelineView {
    pub title: String,
    pub sources: Vec<Source>,
    /// This machine's side has not been read yet.
    pub reading_machine: bool,
    rows: Vec<RowKind>,

    /// The window onto time, easing as it pans and zooms.
    vp: Viewport,

    row: usize,
    item: usize,
    pub info: u8,

    born: Instant,
    fitted: bool,
    drag: Option<(u16, f64)>,
    hits: Vec<(Rect, Hit)>,
    row_y: Vec<(u16, u16, usize)>,
}

impl TimelineView {
    pub fn new(title: impl Into<String>, sources: Vec<Source>, info: u8) -> Self {
        let mut v = TimelineView {
            title: title.into(),
            sources,
            reading_machine: false,
            rows: Vec::new(),
            vp: Viewport::new(day(today()), 3.0, MIN_SCALE, MAX_SCALE),
            row: 0,
            item: 0,
            info: info.min(MAX_INFO),
            born: Instant::now(),
            fitted: false,
            drag: None,
            hits: Vec::new(),
            row_y: Vec::new(),
        };
        v.rebuild_rows();
        v
    }

    fn bundle(&self) -> bool {
        self.sources.len() > 1
    }

    /// Lanes worth a row. A single component shows all of them; a bundle
    /// shows, per member, only what this machine is on or is being offered.
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let bundle = self.bundle();
        for (si, s) in self.sources.iter().enumerate() {
            if let Some(up) = &s.upstream {
                for (li, lane) in up.lanes.iter().enumerate() {
                    if bundle {
                        let mine = s.machine.series.as_deref() == Some(lane.series.as_str());
                        let marked = lane
                            .releases
                            .iter()
                            .any(|r| s.machine.marks_for(&r.version).any());
                        // A date-versioned lane holds whatever is installed,
                        // even when that exact snapshot was never a tarball.
                        let dated =
                            lane.series.is_empty() && s.machine.marks.values().any(|m| m.installed);
                        if !(mine || marked || dated || lane.state == LaneState::Development) {
                            continue;
                        }
                    }
                    rows.push(RowKind::Lane {
                        source: si,
                        lane: li,
                    });
                }
            }
            // In a bundle the lanes already show what is installed; only a
            // boot history earns its own row there.
            if s.boots || (!bundle && !s.machine.marks.is_empty()) {
                rows.push(RowKind::Machine { source: si });
            }
        }
        self.rows = rows;
        self.row = self.row.min(self.rows.len().saturating_sub(1));
    }

    /// Upstream data arrived for a component.
    pub fn set_upstream(&mut self, component: &str, upstream: Upstream) {
        if let Some(s) = self.sources.iter_mut().find(|s| s.component == component) {
            s.upstream = Some(upstream);
        }
        let selected = self.selected();
        self.rebuild_rows();
        if !self.fitted && self.sources.iter().all(|s| s.upstream.is_some()) {
            self.fitted = true;
            self.focus_current();
            self.fit_around_selection();
            // Arrive from slightly zoomed out, so the view settles into place.
            self.vp.scale = self.vp.target_scale * 1.8;
            self.born = Instant::now();
        } else if let Some(sel) = selected {
            self.select_ref(&sel);
        }
    }

    pub fn set_machine(&mut self, component: &str, machine: Machine) {
        if let Some(s) = self.sources.iter_mut().find(|s| s.component == component) {
            s.machine = machine;
        }
        self.rebuild_rows();
    }

    pub fn set_shape(&mut self, component: &str, version: &str, shape: Option<Shape>) {
        if let Some(s) = self.sources.iter_mut().find(|s| s.component == component) {
            s.shapes.insert(
                version.to_string(),
                shape.map_or(ShapeState::Unavailable, ShapeState::Ready),
            );
        }
    }

    pub fn loading(&self) -> bool {
        self.sources.iter().any(|s| s.upstream.is_none())
    }

    /// Whether the next frame should come soon: something is moving.
    pub fn animating(&self) -> bool {
        self.vp.settling() || self.born.elapsed().as_millis() < 900 || self.loading()
    }

    /// Advance the easing by one frame.
    pub fn step(&mut self) {
        self.vp.step();
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    fn lane_items(&self, source: usize, lane: usize) -> Vec<(NaiveDate, ItemRef)> {
        let Some(l) = self.sources[source]
            .upstream
            .as_ref()
            .and_then(|u| u.lanes.get(lane))
        else {
            return Vec::new();
        };
        let mut items: Vec<(NaiveDate, ItemRef)> = l
            .releases
            .iter()
            .enumerate()
            .map(|(index, r)| {
                (
                    r.date,
                    ItemRef::Release {
                        source,
                        lane,
                        index,
                    },
                )
            })
            .chain(l.projections.iter().enumerate().map(|(index, p)| {
                (
                    p.date,
                    ItemRef::Projection {
                        source,
                        lane,
                        index,
                    },
                )
            }))
            .collect();
        items.sort_by_key(|(d, _)| *d);
        items
    }

    fn machine_items(&self, source: usize) -> Vec<(NaiveDate, ItemRef)> {
        let s = &self.sources[source];
        let mut items: Vec<(NaiveDate, ItemRef)> = if s.machine.history.is_empty() {
            // No history log: what is installed now, and since when.
            s.machine
                .marks
                .iter()
                .filter_map(|(v, m)| {
                    m.installed_at.map(|d| {
                        (
                            d,
                            ItemRef::Install {
                                source,
                                version: v.clone(),
                                date: d,
                            },
                        )
                    })
                })
                .collect()
        } else {
            s.machine
                .history
                .iter()
                .map(|c| {
                    let date = c.at.date_naive();
                    let version = c.version.clone();
                    let item = if c.removed {
                        ItemRef::Removal {
                            source,
                            version,
                            date,
                        }
                    } else {
                        ItemRef::Install {
                            source,
                            version,
                            date,
                        }
                    };
                    (date, item)
                })
                .collect()
        };
        items.extend(
            s.machine
                .boots
                .iter()
                .enumerate()
                .filter(|(_, b)| !b.clean)
                .map(|(index, b)| (b.end.date_naive(), ItemRef::Boot { source, index })),
        );
        // Same-day events keep their real order: an install before the boot
        // that ran it, a removal after.
        items.sort_by(|(da, a), (db, b)| self.item_x(*da, a).total_cmp(&self.item_x(*db, b)));
        items
    }

    /// The exact position of an item on the axis: boots, installs and
    /// removals at their real time, releases at their date.
    fn item_x(&self, date: NaiveDate, r: &ItemRef) -> f64 {
        match r {
            ItemRef::Boot { source, index } => self.sources[*source]
                .machine
                .boots
                .get(*index)
                .map_or(day(date), |b| when(b.end)),
            ItemRef::Install {
                source, version, ..
            }
            | ItemRef::Removal {
                source, version, ..
            } => {
                let removed = matches!(r, ItemRef::Removal { .. });
                self.sources[*source]
                    .machine
                    .history
                    .iter()
                    .find(|c| {
                        c.removed == removed && &c.version == version && c.at.date_naive() == date
                    })
                    .map_or(day(date), |c| when(c.at))
            }
            _ => day(date),
        }
    }

    fn items(&self, row: usize) -> Vec<(NaiveDate, ItemRef)> {
        match self.rows.get(row) {
            Some(RowKind::Lane { source, lane }) => self.lane_items(*source, *lane),
            Some(RowKind::Machine { source }) => self.machine_items(*source),
            None => Vec::new(),
        }
    }

    pub fn selected(&self) -> Option<ItemRef> {
        self.items(self.row).get(self.item).map(|(_, r)| r.clone())
    }

    fn selected_date(&self) -> Option<NaiveDate> {
        self.items(self.row).get(self.item).map(|(d, _)| *d)
    }

    fn selected_x(&self) -> Option<f64> {
        self.items(self.row)
            .get(self.item)
            .map(|(d, r)| self.item_x(*d, r))
    }

    fn select_ref(&mut self, target: &ItemRef) {
        for row in 0..self.rows.len() {
            if let Some(i) = self.items(row).iter().position(|(_, r)| r == target) {
                self.row = row;
                self.item = i;
                return;
            }
        }
    }

    /// Select the version this machine is on: the running one if the
    /// component has one, else the current, else the newest release.
    fn focus_current(&mut self) {
        let mut best: Option<(u8, usize, usize)> = None;
        for row in 0..self.rows.len() {
            let RowKind::Lane { source, .. } = self.rows[row] else {
                continue;
            };
            for (i, (_, r)) in self.items(row).iter().enumerate() {
                if let ItemRef::Release {
                    source: s,
                    lane,
                    index,
                } = r
                {
                    let rel =
                        &self.sources[*s].upstream.as_ref().unwrap().lanes[*lane].releases[*index];
                    let m = self.sources[source].machine.marks_for(&rel.version);
                    let score = if m.running && m.current {
                        4
                    } else if m.running {
                        3
                    } else if m.current {
                        2
                    } else {
                        0
                    };
                    if score > 0 && best.is_none_or(|(b, _, _)| score > b) {
                        best = Some((score, row, i));
                    }
                }
            }
        }
        if let Some((_, row, i)) = best {
            self.row = row;
            self.item = i;
        } else {
            self.row = 0;
            self.item = self.items(0).len().saturating_sub(1);
        }
    }

    /// Frame the selection with its lane's recent history and what is coming.
    fn fit_around_selection(&mut self) {
        let Some(d) = self.selected_date() else {
            self.fit_all();
            return;
        };
        let t = today();
        let mut lo = d.min(t) - Duration::days(45);
        let mut hi = d.max(t) + Duration::days(21);
        for s in &self.sources {
            for l in s.upstream.iter().flat_map(|u| u.lanes.iter()) {
                if l.state == LaneState::Development {
                    hi = hi.max(l.last_date().unwrap_or(hi) + Duration::days(7));
                    lo = lo.min(l.first_date().unwrap_or(lo));
                }
            }
        }
        self.frame(lo, hi);
    }

    fn fit_all(&mut self) {
        let dates: Vec<NaiveDate> = self
            .rows
            .iter()
            .enumerate()
            .flat_map(|(i, _)| self.items(i).into_iter().map(|(d, _)| d))
            .collect();
        if let (Some(lo), Some(hi)) = (dates.iter().min(), dates.iter().max()) {
            self.frame(*lo - Duration::days(7), *hi + Duration::days(7));
        }
    }

    fn frame(&mut self, lo: NaiveDate, hi: NaiveDate) {
        self.vp.frame(day(lo), day(hi));
    }

    fn ensure_visible(&mut self) {
        if let Some(x) = self.selected_x() {
            self.vp.ensure_visible(x);
        }
    }

    fn zoom(&mut self, factor: f64, anchor: Option<f64>) {
        let a = anchor
            .or_else(|| self.selected_x())
            .unwrap_or(self.vp.target_center);
        self.vp.zoom(factor, a);
    }

    fn pan(&mut self, columns: f64) {
        self.vp.pan(columns);
    }

    fn step_item(&mut self, delta: isize) {
        let n = self.items(self.row).len();
        if n == 0 {
            return;
        }
        self.item = (self.item as isize + delta).clamp(0, n as isize - 1) as usize;
        self.ensure_visible();
    }

    fn step_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let from = self.selected_x();
        self.row = (self.row as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        let items: Vec<f64> = self
            .items(self.row)
            .iter()
            .map(|(d, r)| self.item_x(*d, r))
            .collect();
        // Move straight up or down the canvas: the nearest item in time,
        // preferring one already on screen over a jump to another month.
        let (lo, hi) = self.vp.visible_days();
        let nearest = |only_visible: bool| {
            items
                .iter()
                .enumerate()
                .filter(|(_, x)| !only_visible || (lo..=hi).contains(*x))
                .min_by(|(_, a), (_, b)| {
                    let at = from.unwrap_or(self.vp.center);
                    (**a - at).abs().total_cmp(&(**b - at).abs())
                })
                .map(|(i, _)| i)
        };
        self.item = nearest(true).or_else(|| nearest(false)).unwrap_or(0);
        self.ensure_visible();
    }

    fn jump_to(&mut self, pred: impl Fn(&Self, &ItemRef) -> bool) -> bool {
        for row in 0..self.rows.len() {
            if let Some(i) = self.items(row).iter().position(|(_, r)| pred(self, r)) {
                self.row = row;
                self.item = i;
                self.ensure_visible();
                return true;
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Handle a key. Returns false for keys the timeline does not use.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = f64::from(self.vp.plot.width.max(20)) / 3.0;
        match key.code {
            KeyCode::Left if shift => self.pan(-page),
            KeyCode::Right if shift => self.pan(page),
            KeyCode::Char('H') => self.pan(-page),
            KeyCode::Char('L') => self.pan(page),
            KeyCode::Left | KeyCode::Char('h') => self.step_item(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_item(1),
            KeyCode::Up => self.step_row(-1),
            KeyCode::Down => self.step_row(1),
            KeyCode::Home => {
                self.item = 0;
                self.ensure_visible();
            }
            KeyCode::End => {
                self.item = self.items(self.row).len().saturating_sub(1);
                self.ensure_visible();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom(1.0 / 1.6, None),
            KeyCode::Char('-') | KeyCode::Char('_') => self.zoom(1.6, None),
            KeyCode::Char(']') => self.info = (self.info + 1).min(MAX_INFO),
            KeyCode::Char('[') => self.info = self.info.saturating_sub(1),
            KeyCode::Enter => self.info = if self.info == MAX_INFO { 1 } else { MAX_INFO },
            KeyCode::Char('f') => self.fit_all(),
            KeyCode::Char('t') => self.vp.target_center = day(today()),
            KeyCode::Char('c') => {
                self.focus_current();
                self.ensure_visible();
            }
            KeyCode::Char('g') => {
                let found = self.jump_to(|v, r| v.marks_of(r).is_some_and(|m| m.gated));
                if !found {
                    self.jump_to(|v, r| {
                        matches!(r, ItemRef::Projection { source, lane, .. }
                            if v.lane(*source, *lane).is_some_and(|l| l.state == LaneState::Development))
                    });
                }
            }
            _ => return false,
        }
        true
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> bool {
        let inside = |r: Rect| {
            m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
        };
        let at = self.vp.at(m.column);
        match m.kind {
            MouseEventKind::ScrollUp => self.zoom(1.0 / 1.25, Some(at)),
            MouseEventKind::ScrollDown => self.zoom(1.25, Some(at)),
            MouseEventKind::ScrollLeft => self.pan(-4.0),
            MouseEventKind::ScrollRight => self.pan(4.0),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, hit)) = self.hits.iter().find(|(r, _)| inside(*r)).copied() {
                    match hit {
                        Hit::ZoomIn => self.zoom(1.0 / 1.6, None),
                        Hit::ZoomOut => self.zoom(1.6, None),
                        Hit::Less => self.info = self.info.saturating_sub(1),
                        Hit::More => self.info = (self.info + 1).min(MAX_INFO),
                        Hit::Fit => self.fit_all(),
                        Hit::Today => self.vp.target_center = day(today()),
                    }
                    return true;
                }
                if !inside(self.vp.plot) {
                    return false;
                }
                self.drag = Some((m.column, self.vp.target_center));
                // Select what was clicked: the nearest item in that row.
                if let Some(&(_, _, row)) = self
                    .row_y
                    .iter()
                    .find(|(y0, h, _)| m.row >= *y0 && m.row < y0 + h)
                {
                    let items = self.items(row);
                    if let Some((i, _)) = items
                        .iter()
                        .enumerate()
                        .min_by(|(_, (a, _)), (_, (b, _))| {
                            (day(*a) - at).abs().total_cmp(&(day(*b) - at).abs())
                        })
                        .filter(|(_, (d, _))| (day(*d) - at).abs() <= 3.0 * self.vp.scale.max(1.0))
                    {
                        self.row = row;
                        self.item = i;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((col, center)) = self.drag {
                    let c =
                        center - f64::from(i32::from(m.column) - i32::from(col)) * self.vp.scale;
                    self.vp.center = c;
                    self.vp.target_center = c;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            _ => return false,
        }
        true
    }

    /// Lanes whose whole history is fetched, so reverts can be tracked: the
    /// series this machine is on, and the ones waiting for it.
    fn watched(&self, source: usize, lane: usize) -> bool {
        let s = &self.sources[source];
        let Some(l) = self.lane(source, lane) else {
            return false;
        };
        s.machine.series.as_deref() == Some(l.series.as_str())
            || l.state == LaneState::Development
            || l.releases
                .iter()
                .any(|r| s.machine.marks_for(&r.version).gated)
    }

    /// Reverts matched to what they undo, across one lane.
    pub fn watch(&self, source: usize, lane: usize) -> BTreeMap<String, crate::timeline::Watch> {
        let s = &self.sources[source];
        let Some(l) = self.lane(source, lane) else {
            return BTreeMap::new();
        };
        let shaped: Vec<(&str, &Shape)> = l
            .releases
            .iter()
            .filter_map(|r| match s.shapes.get(&r.version) {
                Some(ShapeState::Ready(shape)) => Some((r.version.as_str(), shape)),
                _ => None,
            })
            .collect();
        crate::timeline::revert_watch(&shaped)
    }

    /// Releases whose changes should be fetched now: the selection, and at
    /// the richer info levels everything visible in the selected lane.
    pub fn wanted_shapes(&self) -> Vec<(String, String)> {
        if self.info == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut want = |source: usize, version: &str| {
            let s = &self.sources[source];
            if s.has_shapes
                && !s.shapes.contains_key(version)
                && !out.iter().any(|(_, v)| v == version)
            {
                out.push((s.component.clone(), version.to_string()));
            }
        };
        if let Some(ItemRef::Release {
            source,
            lane,
            index,
        }) = self.selected()
        {
            if let Some(r) = self.lane(source, lane).and_then(|l| l.releases.get(index)) {
                want(source, &r.version);
            }
        }
        // Watched lanes are fetched whole, so reverts can be matched.
        for row in &self.rows {
            if let RowKind::Lane { source, lane } = *row {
                if self.watched(source, lane) {
                    if let Some(l) = self.lane(source, lane) {
                        for r in l.releases.iter().filter(|r| r.kind == ReleaseKind::Point) {
                            want(source, &r.version);
                        }
                    }
                }
            }
        }
        if self.info >= 2 {
            if let Some(RowKind::Lane { source, lane }) = self.rows.get(self.row).copied() {
                let (lo, hi) = self.vp.visible_days();
                if let Some(l) = self.lane(source, lane) {
                    for r in l
                        .releases
                        .iter()
                        .filter(|r| (lo..=hi).contains(&day(r.date)))
                    {
                        want(source, &r.version);
                    }
                }
            }
        }
        out
    }

    /// What this timeline knows about one release that a decision should
    /// weigh: its record on this machine, and what it did to this machine's
    /// drivers.
    pub fn release_notes(&self, component: &str, version: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Some(s) = self.sources.iter().find(|s| s.component == component) else {
            return out;
        };
        let key = crate::timeline::normalize(version);
        if let Some(rec) = s.machine.records.get(&key) {
            out.push(format!("already run here: {}", rec.summary()));
        }
        let si = self
            .sources
            .iter()
            .position(|x| x.component == component)
            .unwrap_or(0);
        if let Some(up) = &s.upstream {
            for (li, l) in up.lanes.iter().enumerate() {
                if !l.releases.iter().any(|r| r.version == version) {
                    continue;
                }
                let watch = self.watch(si, li);
                if let Some(w) = watch.get(version).filter(|w| !w.reverted_later.is_empty()) {
                    out.push(format!(
                        "↺ {} of this release's changes to your drivers were reverted later",
                        w.reverted_later.len()
                    ));
                }
                let series_total: usize = watch.values().map(|w| w.reverted_later.len()).sum();
                if series_total > 0 {
                    out.push(format!(
                        "↺ across {} so far, {series_total} change(s) to your drivers have been reverted",
                        if l.series.is_empty() { "its releases" } else { l.series.as_str() }
                    ));
                }
            }
        }
        if let Some(ShapeState::Ready(shape)) = s.shapes.get(version) {
            let hw: Vec<String> = shape
                .relevant
                .iter()
                .filter(|(_, _, t)| *t == Tier::Hardware)
                .take(5)
                .map(|(d, n, _)| format!("{d} {n}"))
                .collect();
            if !hw.is_empty() {
                out.push(format!(
                    "changes to this machine's drivers: {}",
                    hw.join(" · ")
                ));
            }
        }
        out
    }

    pub fn mark_loading(&mut self, component: &str, version: &str) {
        if let Some(s) = self.sources.iter_mut().find(|s| s.component == component) {
            s.shapes.insert(version.to_string(), ShapeState::Loading);
        }
    }

    fn lane(&self, source: usize, lane: usize) -> Option<&crate::timeline::Lane> {
        self.sources.get(source)?.upstream.as_ref()?.lanes.get(lane)
    }

    fn marks_of(&self, r: &ItemRef) -> Option<VersionMarks> {
        match r {
            ItemRef::Release {
                source,
                lane,
                index,
            } => {
                let rel = self.lane(*source, *lane)?.releases.get(*index)?;
                Some(self.sources[*source].machine.marks_for(&rel.version))
            }
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, focused_component: &str) {
        self.hits.clear();
        self.row_y.clear();
        let elapsed = self.born.elapsed().as_millis() as f64;
        let spinner = SPINNER[(elapsed / 90.0) as usize % SPINNER.len()];

        // Frame.
        draw_box(buf, area, if self.bundle() { ACCENT } else { FAINT });
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        if inner.width < 30 || inner.height < 8 {
            put(
                buf,
                inner,
                inner.x,
                inner.y,
                "window too small for the timeline",
                Style::default().fg(MUTED),
            );
            return;
        }

        // Toolbar.
        let title = if self.bundle() {
            format!(" ▣ {} ", self.title)
        } else {
            format!(" {} ", focused_component)
        };
        let mut x = inner.x;
        x = put(
            buf,
            inner,
            x,
            inner.y,
            &title,
            Style::default()
                .fg(WHITE)
                .bg(if self.bundle() {
                    ACCENT
                } else {
                    Color::Rgb(0x33, 0x41, 0x55)
                })
                .add_modifier(Modifier::BOLD),
        );
        x = put(
            buf,
            inner,
            x + 1,
            inner.y,
            "timeline",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        );
        let status = if self.reading_machine {
            format!("  {spinner} reading this machine…")
        } else if self.loading() {
            format!("  {spinner} reading release history…")
        } else {
            let from_cache = self
                .sources
                .iter()
                .any(|s| s.upstream.as_ref().is_some_and(|u| u.from_cache));
            let warnings: usize = self
                .sources
                .iter()
                .filter_map(|s| s.upstream.as_ref())
                .map(|u| u.warnings.len())
                .sum();
            match (from_cache, warnings) {
                (_, w) if w > 0 => format!("  {w} warning(s) — see Log"),
                (true, _) => "  from cache".into(),
                _ => String::new(),
            }
        };
        put(buf, inner, x, inner.y, &status, Style::default().fg(GHOST));

        // Buttons, right-aligned: zoom, info, fit, today.
        let dots: String = (0..=MAX_INFO)
            .map(|i| if i <= self.info { '●' } else { '○' })
            .collect();
        let buttons: Vec<(String, Option<Hit>, Style)> = vec![
            ("[−]".into(), Some(Hit::ZoomOut), Style::default().fg(AUTO)),
            (
                format!(" {} ", zoom_label(self.vp.scale)),
                None,
                Style::default().fg(MUTED),
            ),
            ("[+]".into(), Some(Hit::ZoomIn), Style::default().fg(AUTO)),
            ("  ".into(), None, Style::default()),
            (
                "[◂ less]".into(),
                Some(Hit::Less),
                Style::default().fg(AUTO),
            ),
            (format!(" {dots} "), None, Style::default().fg(ACCENT)),
            (
                "[more ▸]".into(),
                Some(Hit::More),
                Style::default().fg(AUTO),
            ),
            ("  ".into(), None, Style::default()),
            ("[fit]".into(), Some(Hit::Fit), Style::default().fg(AUTO)),
            (" ".into(), None, Style::default()),
            (
                "[today]".into(),
                Some(Hit::Today),
                Style::default().fg(TODAY),
            ),
        ];
        let total: u16 = buttons
            .iter()
            .map(|(t, _, _)| t.chars().count() as u16)
            .sum();
        let mut bx = (inner.x + inner.width).saturating_sub(total);
        for (text, hit, style) in buttons {
            let w = text.chars().count() as u16;
            put(buf, inner, bx, inner.y, &text, style);
            if let Some(h) = hit {
                self.hits.push((
                    Rect {
                        x: bx,
                        y: inner.y,
                        width: w,
                        height: 1,
                    },
                    h,
                ));
            }
            bx += w;
        }

        // A legend, so the glyphs can be learned by looking.
        let legend: [(&str, &str, Style); 10] = [
            ("►", "running", Style::default().fg(AUTO)),
            ("●", "installed", Style::default().fg(GOOD)),
            ("★", "boots by default", Style::default().fg(STAR)),
            ("✓", "known-good", Style::default().fg(GOOD)),
            ("◆", "gated", Style::default().fg(GATED)),
            ("◉", "offered", Style::default().fg(AUTO)),
            ("◌", "expected", Style::default().fg(GHOST)),
            ("✘", "unclean end", Style::default().fg(BAD)),
            (
                "◍",
                "was installed",
                Style::default().fg(mix(GOOD, FAINT, 0.45)),
            ),
            ("↺", "later reverted", Style::default().fg(GATED)),
        ];
        let mut lx = inner.x + 1;
        for (glyph, what, style) in legend {
            let need = (glyph.chars().count() + what.chars().count() + 3) as u16;
            if lx + need >= inner.x + inner.width {
                break;
            }
            lx = put(buf, inner, lx, inner.y + 1, glyph, style);
            lx = put(
                buf,
                inner,
                lx + 1,
                inner.y + 1,
                what,
                Style::default().fg(FAINT),
            );
            lx += 2;
        }

        // Geometry.
        let label_w: u16 = self
            .rows
            .iter()
            .map(|r| self.row_label(*r).chars().count() as u16 + 2)
            .max()
            .unwrap_or(8)
            .clamp(8, 24);
        let card_h: u16 = match self.info {
            0 => 5,
            1 => 6,
            2 => 8,
            _ => 14,
        }
        .min(inner.height / 2);
        let lane_h: u16 = match self.info {
            0 => 1,
            1 => 2,
            _ => 3,
        };
        let axis_y = inner.y + 2;
        let rows_top = axis_y + 2;
        let rows_bottom = (inner.y + inner.height).saturating_sub(card_h + 1);
        self.vp.plot = Rect {
            x: inner.x + label_w,
            y: axis_y,
            width: inner.width.saturating_sub(label_w + 1),
            height: rows_bottom.saturating_sub(axis_y),
        };
        if !self.fitted && self.loading() {
            // A sensible frame while waiting: the last few months.
            self.frame(today() - Duration::days(120), today() + Duration::days(30));
            self.vp.center = self.vp.target_center;
            self.vp.scale = self.vp.target_scale;
        }

        // The intro: the canvas draws itself from left to right.
        let reveal = ease_out(elapsed / 700.0);
        let reveal_col = self.vp.plot.x + (f64::from(self.vp.plot.width) * reveal) as u16;

        self.vp.draw_axis(buf, axis_y);

        // Rows, scrolled so the selected one stays visible.
        let heights: Vec<u16> = self
            .rows
            .iter()
            .map(|r| match r {
                RowKind::Lane { .. } => lane_h,
                RowKind::Machine { .. } => 2,
            })
            .collect();
        let available = rows_bottom.saturating_sub(rows_top);
        let mut first = 0;
        while first < self.row && heights[first..=self.row].iter().sum::<u16>() > available {
            first += 1;
        }
        let mut y = rows_top;
        for (ri, &h) in heights.iter().enumerate().skip(first) {
            if y + h > rows_bottom {
                break;
            }
            self.row_y.push((y, h, ri));
            let selected_row = ri == self.row;
            let label = self.row_label(self.rows[ri]);
            let label_style = self.row_label_style(self.rows[ri], selected_row);
            let marker_y = y + if h >= 3 { 1 } else { 0 };
            if selected_row {
                put(
                    buf,
                    inner,
                    inner.x,
                    marker_y,
                    "▸",
                    Style::default().fg(ACCENT),
                );
            }
            put(buf, inner, inner.x + 1, marker_y, &label, label_style);
            match self.rows[ri] {
                RowKind::Lane { source, lane } => self.draw_lane(
                    buf,
                    (source, lane),
                    Band {
                        y,
                        h,
                        reveal_col,
                        elapsed,
                    },
                ),
                RowKind::Machine { source } => self.draw_machine(buf, source, y, reveal_col),
            }
            y += h;
        }
        if first > 0 {
            put(
                buf,
                inner,
                inner.x,
                rows_top,
                "▲",
                Style::default().fg(GHOST),
            );
        }

        self.draw_today(buf, axis_y, rows_bottom);
        self.draw_cursor(buf, axis_y, rows_bottom);

        // The intro's leading edge.
        if reveal < 0.999 && reveal_col < self.vp.plot.x + self.vp.plot.width {
            for yy in rows_top..rows_bottom {
                let cell = &mut buf[(reveal_col, yy)];
                cell.set_char('▏').set_fg(ACCENT);
            }
        }

        let card = Rect {
            x: inner.x,
            y: rows_bottom,
            width: inner.width,
            height: (inner.y + inner.height).saturating_sub(rows_bottom),
        };
        self.draw_card(buf, card, spinner);
    }

    fn row_label(&self, r: RowKind) -> String {
        let bundle = self.bundle();
        match r {
            RowKind::Lane { source, lane } => {
                let s = &self.sources[source];
                let l = &s.upstream.as_ref().unwrap().lanes[lane];
                match (bundle, l.series.is_empty()) {
                    (true, true) => s.component.clone(),
                    (true, false) => format!("{} {}", s.component, l.series),
                    (false, true) => "releases".to_string(),
                    (false, false) => l.series.clone(),
                }
            }
            RowKind::Machine { source } => {
                let s = &self.sources[source];
                let name = if s.boots { "boots" } else { "installs" };
                if bundle {
                    format!("{} {name}", s.component)
                } else {
                    name.to_string()
                }
            }
        }
    }

    fn row_label_style(&self, r: RowKind, selected: bool) -> Style {
        let base = match r {
            RowKind::Lane { source, lane } => {
                let s = &self.sources[source];
                let l = &s.upstream.as_ref().unwrap().lanes[lane];
                if s.machine.series.as_deref() == Some(l.series.as_str()) {
                    Style::default().fg(AUTO).add_modifier(Modifier::BOLD)
                } else {
                    match l.state {
                        LaneState::Development => {
                            Style::default().fg(GHOST).add_modifier(Modifier::ITALIC)
                        }
                        LaneState::Eol => Style::default().fg(FAINT),
                        LaneState::Longterm => Style::default().fg(GOOD),
                        LaneState::Active => Style::default().fg(MUTED),
                    }
                }
            }
            RowKind::Machine { .. } => Style::default().fg(STAR).add_modifier(Modifier::ITALIC),
        };
        if selected {
            base.add_modifier(Modifier::UNDERLINED)
        } else {
            base
        }
    }

    fn draw_lane(&self, buf: &mut Buffer, (source, lane): (usize, usize), band: Band) {
        let Band {
            y,
            h,
            reveal_col,
            elapsed,
        } = band;
        let s = &self.sources[source];
        let l = &s.upstream.as_ref().unwrap().lanes[lane];
        let mine = s.machine.series.as_deref() == Some(l.series.as_str());
        let marker_y = y + if h >= 3 { 1 } else { 0 };
        let label_y = marker_y + 1;
        let heat_y = y;
        let t = today();

        // The lane's life: solid while released, dashed where it is still
        // being developed or where its next release is expected.
        let (line_char, line_style) = match (mine, l.state) {
            (true, _) => ('━', Style::default().fg(AUTO)),
            (_, LaneState::Development) => ('┄', Style::default().fg(GHOST)),
            (_, LaneState::Eol) => ('─', Style::default().fg(FAINT)),
            (_, LaneState::Longterm) => ('─', Style::default().fg(GOOD)),
            (_, LaneState::Active) => ('─', Style::default().fg(MUTED)),
        };
        let start = l.first_date();
        let solid_end = match l.state {
            LaneState::Eol => l.end.or_else(|| l.releases.last().map(|r| r.date)),
            LaneState::Development => l.releases.last().map(|r| r.date),
            _ => Some(t),
        };
        if let (Some(a), Some(b)) = (start, solid_end) {
            self.hline(
                buf,
                (day(a), day(b)),
                marker_y,
                (line_char, line_style),
                reveal_col,
            );
        }
        // Ghost stretch to each projection, marching gently.
        let phase = (elapsed / 160.0) as u16;
        for p in &l.projections {
            let from = solid_end.map_or(day(t), day);
            if let (Some(c0), Some(c1)) = (
                self.vp.col(from.min(day(p.date))),
                self.vp.col(day(p.date).max(from)),
            ) {
                for c in c0..=c1 {
                    if c >= reveal_col {
                        break;
                    }
                    let cell = &mut buf[(c, marker_y)];
                    if cell.symbol() == " " {
                        let ch = if (c + phase).is_multiple_of(3) {
                            '·'
                        } else {
                            '┄'
                        };
                        cell.set_char(ch).set_fg(FAINT);
                    }
                }
            }
        }
        if l.state == LaneState::Eol {
            if let Some(c) = l.end.and_then(|e| self.vp.col(day(e) + self.vp.scale)) {
                if c < reveal_col {
                    buf[(c, marker_y)].set_char('┤').set_fg(FAINT);
                    if h >= 2 && self.info >= 1 {
                        put(
                            buf,
                            self.vp.plot,
                            c + 1,
                            marker_y,
                            "EOL",
                            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
                        );
                    }
                }
            }
        }

        // A lane entirely out of view points to where it is.
        let (vis_lo, vis_hi) = self.vp.visible_days();
        if let (Some(a), Some(b)) = (l.first_date(), l.last_date()) {
            let style = Style::default().fg(GHOST);
            if day(b) < vis_lo {
                put(
                    buf,
                    self.vp.plot,
                    self.vp.plot.x,
                    marker_y,
                    &format!("◂ {b}"),
                    style,
                );
            } else if day(a) > vis_hi {
                let text = format!("{a} ▸");
                let x = (self.vp.plot.x + self.vp.plot.width)
                    .saturating_sub(text.chars().count() as u16);
                put(buf, self.vp.plot, x, marker_y, &text, style);
            }
        }

        // Date-versioned packages built from a snapshot (firmware at
        // `20260829`) match no upstream tarball, but the version is a date, so
        // they still have a place on the lane.
        if l.series.is_empty() {
            for (v, m) in &s.machine.marks {
                if !(m.installed || m.known_good) || l.releases.iter().any(|r| r.version == *v) {
                    continue;
                }
                let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") else {
                    continue;
                };
                let Some(c) = self.vp.col(day(d)) else {
                    continue;
                };
                if c >= reveal_col {
                    continue;
                }
                let r = Release {
                    version: v.clone(),
                    date: d,
                    kind: ReleaseKind::Point,
                    inferred: false,
                };
                let (glyph, style) = release_glyph(&r, m, true, elapsed);
                buf[(c, marker_y)].set_char(glyph).set_style(style);
                if h >= 2 && self.info >= 1 {
                    put(
                        buf,
                        self.vp.plot,
                        c,
                        label_y,
                        v,
                        Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
                    );
                }
            }
        }

        let watch = self.watch(source, lane);

        // Markers, most important last so they win shared columns.
        let mut order: Vec<(u8, usize)> = l
            .releases
            .iter()
            .enumerate()
            .map(|(i, r)| (priority(&s.machine.marks_for(&r.version), r), i))
            .collect();
        order.sort();

        // Badges under each marker: what this machine does with it. Worked out
        // first, so labels can be placed around every release's badges.
        let badges_of = |r: &Release| -> String {
            let marks = s.machine.marks_for(&r.version);
            let mut b = String::new();
            if marks.default_boot {
                b.push('★');
            }
            if marks.known_good {
                b.push('✓');
            }
            if marks.testing {
                b.push('◐');
            }
            if marks.vaulted && self.info >= 2 {
                b.push('▣');
            }
            if s.machine
                .records
                .get(&crate::timeline::normalize(&r.version))
                .is_some_and(|x| x.unclean > 0)
            {
                b.push('✘');
            }
            if watch
                .get(&r.version)
                .is_some_and(|w| !w.reverted_later.is_empty())
            {
                b.push('↺');
            }
            b
        };
        let badge_cells: Vec<(u16, u16)> = l
            .releases
            .iter()
            .filter_map(|r| {
                let n = badges_of(r).chars().count() as u16;
                let c = self.vp.col(day(r.date))?;
                (n > 0).then(|| (c, c + n - 1))
            })
            .collect();

        let mut occupied: Vec<(u16, u16)> = Vec::new();
        for (_, i) in order {
            let r = &l.releases[i];
            let Some(c) = self.vp.col(day(r.date)) else {
                continue;
            };
            if c >= reveal_col {
                continue;
            }
            let marks = s.machine.marks_for(&r.version);
            let (glyph, style) = release_glyph(r, &marks, mine, elapsed);
            buf[(c, marker_y)].set_char(glyph).set_style(style);

            // Badges under the marker: what this machine does with it.
            if h >= 2 {
                let badges = badges_of(r);
                let mut bx = c;
                for ch in badges.chars() {
                    let color = match ch {
                        '★' => STAR,
                        '✓' => GOOD,
                        '◐' => AUTO,
                        '✘' => BAD,
                        '↺' => GATED,
                        _ => MUTED,
                    };
                    if bx < self.vp.plot.x + self.vp.plot.width {
                        buf[(bx, label_y)].set_char(ch).set_fg(color);
                        bx += 1;
                    }
                }
                let important = marks.any() || r.kind == ReleaseKind::Mainline;
                if self.info >= 1 && (important || self.vp.scale < 2.5) {
                    let text = short_version(&r.version, &l.series);
                    let x0 = if bx > c { bx } else { c };
                    let x1 = x0 + text.chars().count() as u16;
                    let hits_badge = badge_cells
                        .iter()
                        .any(|(a, b)| *a != c && x0 <= *b && *a <= x1);
                    if !occupied.iter().any(|(a, b)| x0 <= *b && *a <= x1)
                        && !hits_badge
                        && x1 < self.vp.plot.x + self.vp.plot.width
                    {
                        let st = if marks.running || marks.current {
                            Style::default().fg(WHITE).add_modifier(Modifier::BOLD)
                        } else if marks.installed {
                            Style::default().fg(GOOD)
                        } else if r.kind == ReleaseKind::Candidate {
                            Style::default().fg(GHOST).add_modifier(Modifier::ITALIC)
                        } else {
                            Style::default().fg(MUTED)
                        };
                        put(buf, self.vp.plot, x0, label_y, &text, st);
                        occupied.push((x0.saturating_sub(1), x1));
                    }
                }
            }

            // The shape of the release, as a bar above it: how much changed,
            // and how much of that touches this machine.
            if h >= 3 {
                if let Some(ShapeState::Ready(shape)) = s.shapes.get(&r.version) {
                    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
                    let level = ((shape.patches as f64 + 1.0).log10() / 3.5 * 7.0)
                        .round()
                        .clamp(0.0, 7.0) as usize;
                    let hardware: usize = shape
                        .relevant
                        .iter()
                        .filter(|(_, _, t)| *t == Tier::Hardware)
                        .map(|(_, n, _)| *n)
                        .sum();
                    let heat = if shape.patches == 0 {
                        0.0
                    } else {
                        (hardware as f64 / shape.patches as f64 * 8.0).min(1.0)
                    };
                    buf[(c, heat_y)]
                        .set_char(bars[level])
                        .set_fg(mix(FAINT, GATED, heat));
                } else if matches!(s.shapes.get(&r.version), Some(ShapeState::Loading)) {
                    let spin = ['·', '∙', '•', '∙'][((elapsed / 120.0) as usize + i) % 4];
                    buf[(c, heat_y)].set_char(spin).set_fg(FAINT);
                }
            }
        }

        // Projections: hollow, grey, and labelled as expectations.
        for p in &l.projections {
            let Some(c) = self.vp.col(day(p.date)) else {
                continue;
            };
            if c >= reveal_col {
                continue;
            }
            let breathe = 0.5 + 0.5 * (elapsed / 700.0).sin();
            let color = mix(FAINT, GHOST, breathe);
            let glyph = match p.kind {
                ProjectionKind::NextMainline => '◇',
                _ => '◌',
            };
            buf[(c, marker_y)].set_char(glyph).set_fg(color);
            if h >= 2 && self.info >= 1 {
                let text = format!("{}?", short_version(&p.label, &l.series));
                let x1 = c + text.chars().count() as u16;
                if !occupied.iter().any(|(a, b)| c <= *b && *a <= x1) {
                    put(
                        buf,
                        self.vp.plot,
                        c,
                        label_y,
                        &text,
                        Style::default().fg(GHOST).add_modifier(Modifier::ITALIC),
                    );
                    occupied.push((c, x1));
                }
            }
            // The uncertainty, at the richer levels.
            if self.info >= 2 && h >= 3 {
                let lo = self.vp.col(day(p.date) - p.spread_days as f64);
                let hi = self.vp.col(day(p.date) + p.spread_days as f64);
                if let (Some(a), Some(b)) = (lo, hi) {
                    for x in a..=b {
                        let cell = &mut buf[(x, heat_y)];
                        if cell.symbol() == " " {
                            cell.set_char('╌').set_fg(FAINT);
                        }
                    }
                }
            }
        }
    }

    fn draw_machine(&self, buf: &mut Buffer, source: usize, y: u16, reveal_col: u16) {
        let s = &self.sources[source];
        // Scrubbing a kernel lights up the boots that ran it.
        let selected_version = match self.selected() {
            Some(ItemRef::Release {
                source: rs,
                lane,
                index,
            }) if rs == source => self
                .lane(rs, lane)
                .and_then(|l| l.releases.get(index))
                .map(|r| crate::timeline::normalize(&r.version)),
            Some(ItemRef::Install {
                source: rs,
                version,
                ..
            })
            | Some(ItemRef::Removal {
                source: rs,
                version,
                ..
            }) if rs == source => Some(version),
            _ => None,
        };
        // Boots as a strip: each boot a run of blocks, a freeze a red cross.
        for b in &s.machine.boots {
            let from = when(b.start);
            let to = when(b.end);
            let lit = selected_version.is_some() && b.version() == selected_version;
            let (ch, color) = if lit {
                (
                    '▅',
                    if b.kernel_inferred {
                        mix(AUTO, FAINT, 0.35)
                    } else {
                        AUTO
                    },
                )
            } else {
                ('▂', Color::Rgb(0x16, 0x65, 0x34))
            };
            if let (Some(c0), Some(c1)) =
                (self.vp.col(from).or(Some(self.vp.plot.x)), self.vp.col(to))
            {
                for c in c0..=c1 {
                    if c >= reveal_col || c >= self.vp.plot.x + self.vp.plot.width {
                        break;
                    }
                    let cell = &mut buf[(c, y)];
                    if cell.symbol() != "✘" {
                        cell.set_char(ch).set_fg(color);
                    }
                }
            }
            if !b.clean {
                if let Some(c) = self.vp.col(to) {
                    if c < reveal_col {
                        let color = if b.pstore_hits > 0 { WHITE } else { BAD };
                        buf[(c, y)]
                            .set_char('✘')
                            .set_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
                    }
                }
            }
        }
        if !s.boots {
            for c in self.vp.plot.x..self.vp.plot.x + self.vp.plot.width {
                if c < reveal_col {
                    buf[(c, y)].set_char('·').set_fg(FAINT);
                }
            }
        }
        // Installs (▼) and removals (▽): when each version came and went.
        let mut occupied: Vec<(u16, u16)> = Vec::new();
        let events: Vec<(f64, String, bool)> = if s.machine.history.is_empty() {
            s.machine
                .marks
                .iter()
                .filter_map(|(v, m)| m.installed_at.map(|d| (day(d), v.clone(), false)))
                .collect()
        } else {
            s.machine
                .history
                .iter()
                .map(|c| (when(c.at), c.version.clone(), c.removed))
                .collect()
        };
        for (x, v, removed) in events {
            let Some(c) = self.vp.col(x) else { continue };
            if c >= reveal_col {
                continue;
            }
            let m = s.machine.marks.get(&v).cloned().unwrap_or_default();
            let (glyph, color) = if removed {
                ('▽', mix(BAD, FAINT, 0.4))
            } else if m.running {
                ('▼', AUTO)
            } else {
                ('▼', GOOD)
            };
            buf[(c, y)].set_char(glyph).set_fg(color);
            if self.info >= 1 {
                let text = if removed {
                    format!("−{v}")
                } else {
                    v.clone()
                };
                let x1 = c + text.chars().count() as u16;
                if !occupied.iter().any(|(a, b)| c <= *b && *a <= x1) {
                    put(
                        buf,
                        self.vp.plot,
                        c,
                        y + 1,
                        &text,
                        Style::default().fg(color),
                    );
                    occupied.push((c, x1));
                }
            }
        }
    }

    fn hline(
        &self,
        buf: &mut Buffer,
        (from, to): (f64, f64),
        y: u16,
        (ch, style): (char, Style),
        reveal_col: u16,
    ) {
        let (lo, hi) = self.vp.visible_days();
        if to < lo || from > hi {
            return;
        }
        let a = self.vp.col(from.max(lo)).unwrap_or(self.vp.plot.x);
        let b = self
            .vp
            .col(to.min(hi))
            .unwrap_or(self.vp.plot.x + self.vp.plot.width - 1);
        for c in a..=b {
            if c >= reveal_col {
                break;
            }
            buf[(c, y)].set_char(ch).set_style(style);
        }
    }

    fn draw_today(&self, buf: &mut Buffer, axis_y: u16, bottom: u16) {
        let Some(c) = self.vp.col(day(today())) else {
            return;
        };
        put(
            buf,
            self.vp.plot,
            c.saturating_sub(3),
            axis_y,
            " today ",
            Style::default().fg(TODAY).add_modifier(Modifier::BOLD),
        );
        for y in axis_y + 1..bottom {
            let cell = &mut buf[(c, y)];
            match cell.symbol() {
                " " | "─" | "━" | "┄" | "·" => {
                    cell.set_char('│').set_fg(mix(TODAY, CURSOR_BG, 0.45));
                }
                _ => {}
            }
        }
    }

    fn draw_cursor(&self, buf: &mut Buffer, axis_y: u16, bottom: u16) {
        let (Some(d), Some(x)) = (self.selected_date(), self.selected_x()) else {
            return;
        };
        let Some(c) = self.vp.col(x) else { return };
        for y in axis_y + 2..bottom {
            buf[(c, y)].set_bg(CURSOR_BG);
        }
        // The selection on its own row gets a halo.
        if let Some(&(y0, h, _)) = self.row_y.iter().find(|(_, _, r)| *r == self.row) {
            let marker_y = y0 + if h >= 3 { 1 } else { 0 };
            buf[(c, marker_y)].set_bg(ACCENT).set_fg(Color::Black);
        }
        let label = d.format(" %Y-%m-%d ").to_string();
        let x = c.saturating_sub(label.chars().count() as u16 / 2);
        put_within(
            buf,
            self.vp.plot,
            x,
            axis_y + 1,
            &label,
            Style::default().fg(Color::Black).bg(ACCENT),
        );
    }

    fn draw_card(&self, buf: &mut Buffer, area: Rect, spinner: char) {
        if area.height < 3 {
            return;
        }
        for x in area.x..area.x + area.width {
            buf[(x, area.y)].set_char('─').set_fg(FAINT);
        }
        let body = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height - 1,
        };
        let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
        let dim = Style::default().fg(MUTED);
        let t = today();
        let when = |d: NaiveDate| -> String {
            let n = (d - t).num_days();
            match n {
                0 => "today".into(),
                n if n > 0 => format!("in {n} day(s)"),
                n => format!("{} day(s) ago", -n),
            }
        };

        match self.selected() {
            None => lines.push(vec![(
                if self.loading() {
                    format!("{spinner} fetching upstream release history…")
                } else {
                    "nothing to show for this component".into()
                },
                dim,
            )]),
            Some(ItemRef::Release {
                source,
                lane,
                index,
            }) => {
                let s = &self.sources[source];
                let l = &s.upstream.as_ref().unwrap().lanes[lane];
                let r = &l.releases[index];
                let m = s.machine.marks_for(&r.version);
                let mine = s.machine.series.as_deref() == Some(l.series.as_str());
                let (glyph, gstyle) = release_glyph(r, &m, mine, 0.0);
                let kind = match r.kind {
                    ReleaseKind::Mainline => "first release of the series",
                    ReleaseKind::Point => "point release",
                    ReleaseKind::Candidate if r.inferred => "release candidate (date inferred)",
                    ReleaseKind::Candidate => "release candidate",
                };
                lines.push(vec![
                    (format!("{glyph} "), gstyle),
                    (
                        format!("{} {}", s.component, r.version),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (format!("   {kind} · {} · {}", r.date, when(r.date)), dim),
                ]);
                lines.push(chips(&m));
                if self.info >= 1 {
                    let lane_note = match l.state {
                        LaneState::Development => format!("series {} is in development", l.series),
                        LaneState::Active if mine => {
                            format!("series {} — your series, supported upstream", l.series)
                        }
                        LaneState::Active => format!("series {} is supported upstream", l.series),
                        LaneState::Longterm => format!("series {} is a longterm series", l.series),
                        LaneState::Eol => match l.end {
                            Some(e) => {
                                format!("series {} stopped receiving releases on {e}", l.series)
                            }
                            None => format!("series {} is end-of-life", l.series),
                        },
                    };
                    let mut v = vec![(lane_note, dim)];
                    if let Some(d) = m.installed_at {
                        v.push((
                            format!("  ·  installed here {d}"),
                            Style::default().fg(GOOD),
                        ));
                    }
                    lines.push(v);
                }
                // What happened to its changes afterwards.
                if let Some(w) = self.watch(source, lane).get(&r.version) {
                    if !w.reverted_later.is_empty() {
                        let mut by: Vec<&str> =
                            w.reverted_later.iter().map(|x| x.by.as_str()).collect();
                        by.dedup();
                        let mut drivers: Vec<&str> =
                            w.reverted_later.iter().map(|x| x.driver.as_str()).collect();
                        drivers.sort_unstable();
                        drivers.dedup();
                        lines.push(vec![(
                            format!(
                                "↺ {} of its changes to {} {} reverted later, in {}",
                                w.reverted_later.len(),
                                drivers.join(", "),
                                if w.reverted_later.len() == 1 {
                                    "was"
                                } else {
                                    "were"
                                },
                                by.join(", ")
                            ),
                            Style::default().fg(GATED).add_modifier(Modifier::BOLD),
                        )]);
                        if self.info >= 3 {
                            for x in &w.reverted_later {
                                lines.push(vec![
                                    ("  ↺ ".into(), Style::default().fg(GATED)),
                                    (x.subject.clone(), Style::default().fg(WHITE)),
                                    (format!("  — reverted in {}", x.by), dim),
                                ]);
                            }
                        }
                    }
                    if !w.reverts_earlier.is_empty() {
                        let mut from: Vec<&str> =
                            w.reverts_earlier.iter().map(|x| x.2.as_str()).collect();
                        from.dedup();
                        lines.push(vec![(
                            format!(
                                "reverts {} earlier change{} to this machine's drivers (from {})",
                                w.reverts_earlier.len(),
                                if w.reverts_earlier.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                },
                                from.join(", ")
                            ),
                            Style::default().fg(MUTED),
                        )]);
                    }
                }
                // How this kernel has behaved on this machine.
                if let Some(rec) = s
                    .machine
                    .records
                    .get(&crate::timeline::normalize(&r.version))
                {
                    let color = if rec.unclean > 0 { BAD } else { GOOD };
                    lines.push(vec![
                        ("on this machine: ".into(), Style::default().fg(STAR)),
                        (
                            rec.summary(),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                    ]);
                }
                if self.info >= 2 {
                    match s.shapes.get(&r.version) {
                        Some(ShapeState::Ready(shape)) => {
                            let mut v = vec![(
                                format!("{} changes · {} revert(s)", shape.patches, shape.reverts),
                                Style::default().fg(WHITE),
                            )];
                            for (label, n) in &shape.highlights {
                                v.push((format!(" · {label} {n}"), dim));
                            }
                            lines.push(v);
                            if !shape.relevant.is_empty() {
                                let mut v = vec![(
                                    "touches this machine: ".to_string(),
                                    Style::default().fg(GATED),
                                )];
                                for (i, (driver, n, tier)) in
                                    shape.relevant.iter().take(10).enumerate()
                                {
                                    if i > 0 {
                                        v.push((" · ".into(), dim));
                                    }
                                    // Drivers for this machine's devices stand out;
                                    // merely loaded modules do not.
                                    let st = match tier {
                                        Tier::Hardware => {
                                            Style::default().fg(AUTO).add_modifier(Modifier::BOLD)
                                        }
                                        Tier::Loaded => Style::default().fg(MUTED),
                                    };
                                    v.push((format!("{driver} {n}"), st));
                                }
                                lines.push(v);
                            } else if shape.patches > 0 {
                                lines.push(vec![(
                                    "nothing in it touches this machine's drivers".into(),
                                    Style::default().fg(GOOD),
                                )]);
                            }
                            if self.info >= 3 {
                                lines.push(vec![(
                                    "what changed that matters here:".into(),
                                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                                )]);
                                for n in &shape.notable {
                                    lines.push(vec![
                                        ("  • ".into(), Style::default().fg(GATED)),
                                        (n.clone(), Style::default().fg(WHITE)),
                                    ]);
                                }
                            }
                        }
                        Some(ShapeState::Loading) => {
                            lines.push(vec![(format!("{spinner} reading what changed…"), dim)]);
                        }
                        Some(ShapeState::Unavailable) => lines.push(vec![(
                            if r.kind == ReleaseKind::Mainline {
                                "a first release is the whole merge window — too large to summarise"
                                    .into()
                            } else {
                                "no changelog available for this release".into()
                            },
                            dim,
                        )]),
                        None if !s.has_shapes => lines.push(vec![(
                            "upstream publishes no per-release changelog here".into(),
                            dim,
                        )]),
                        None => {}
                    }
                }
            }
            Some(ItemRef::Projection {
                source,
                lane,
                index,
            }) => {
                let s = &self.sources[source];
                let l = &s.upstream.as_ref().unwrap().lanes[lane];
                let p: &Projection = &l.projections[index];
                let what = match p.kind {
                    ProjectionKind::NextPoint => "next point release",
                    ProjectionKind::NextCandidate => "next release candidate",
                    ProjectionKind::NextMainline => "first release of the series",
                };
                lines.push(vec![
                    ("◌ ".into(), Style::default().fg(GHOST)),
                    (
                        format!("{} {}", s.component, p.label),
                        Style::default()
                            .fg(GHOST)
                            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    ),
                    (
                        format!(
                            "   {what} · expected around {} (± {} day(s)) · {}",
                            p.date,
                            p.spread_days,
                            when(p.date)
                        ),
                        dim,
                    ),
                ]);
                lines.push(vec![(
                    "not released yet — placed by the cadence of the releases before it, so treat the date as a guess".into(),
                    Style::default().fg(GHOST).add_modifier(Modifier::ITALIC),
                )]);
                if l.state == LaneState::Development {
                    if let Some(last) = l.releases.iter().rfind(|r| !r.inferred) {
                        lines.push(vec![(
                            format!("in development: {} came out {}", last.version, last.date),
                            dim,
                        )]);
                    }
                }
            }
            Some(ItemRef::Install {
                source,
                version,
                date,
            }) => {
                let s = &self.sources[source];
                let m = s.machine.marks.get(&version).cloned().unwrap_or_default();
                lines.push(vec![
                    ("▼ ".into(), Style::default().fg(GOOD)),
                    (
                        format!("{} {version}", s.component),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (
                        format!("   installed on this machine {date} · {}", when(date)),
                        dim,
                    ),
                ]);
                lines.push(chips(&m));
            }
            Some(ItemRef::Removal {
                source,
                version,
                date,
            }) => {
                let s = &self.sources[source];
                lines.push(vec![
                    ("▽ ".into(), Style::default().fg(BAD)),
                    (
                        format!("{} {version}", s.component),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (
                        format!("   removed from this machine {date} · {}", when(date)),
                        dim,
                    ),
                ]);
                if let Some(rec) = s.machine.records.get(&version) {
                    lines.push(vec![
                        ("while it was here: ".into(), dim),
                        (rec.summary(), Style::default().fg(WHITE)),
                    ]);
                }
            }
            Some(ItemRef::Boot { source, index }) => {
                let b = &self.sources[source].machine.boots[index];
                let hours = (b.end - b.start).num_minutes() as f64 / 60.0;
                lines.push(vec![
                    (
                        "✘ ".into(),
                        Style::default().fg(BAD).add_modifier(Modifier::BOLD),
                    ),
                    (
                        "a boot that did not shut down".into(),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (
                        format!(
                            "   {} → {} · {hours:.0} h",
                            b.start
                                .with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M"),
                            b.end.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                        ),
                        dim,
                    ),
                ]);
                lines.push(vec![(
                    format!(
                        "kernel {}{} · the journal ends without a shutdown sequence: a freeze, a crash or a power loss{}",
                        b.kernel.as_deref().unwrap_or("unknown (a root `sluice check` records it)"),
                        if b.kernel_inferred { " (inferred from install history)" } else { "" },
                        if b.pstore_hits > 0 { format!(" · {} pstore crash record(s)", b.pstore_hits) } else { String::new() }
                    ),
                    dim,
                )]);
            }
        }

        let hint = "←→ scrub  ↑↓ lane  ⇧←→/drag pan  +/−/wheel zoom  [ ] info  c current  g gated  t today  f fit  j/k component";
        for (i, spans) in lines.iter().enumerate() {
            let y = body.y + i as u16;
            if y + 1 >= body.y + body.height {
                break;
            }
            let mut x = body.x;
            for (text, style) in spans {
                x = put(buf, body, x, y, text, *style);
            }
        }
        put(
            buf,
            body,
            body.x,
            body.y + body.height - 1,
            hint,
            Style::default().fg(FAINT),
        );
    }
}

/// `7.2.6` in lane `7.2` reads as `.6` when space is short, but the full
/// version is clearer, so it is kept unless it is a candidate.
fn short_version(version: &str, series: &str) -> String {
    if !series.is_empty() {
        if let Some(rc) = version.strip_prefix(&format!("{series}-")) {
            return rc.to_string();
        }
    }
    version.to_string()
}

fn priority(m: &VersionMarks, r: &Release) -> u8 {
    if m.running {
        6
    } else if m.gated {
        5
    } else if m.default_boot || m.known_good || m.testing {
        4
    } else if m.installed || m.removed_at.is_some() {
        3
    } else if r.kind == ReleaseKind::Mainline {
        2
    } else if m.offered {
        1
    } else {
        0
    }
}

fn release_glyph(r: &Release, m: &VersionMarks, mine: bool, elapsed: f64) -> (char, Style) {
    let pulse = 0.5 + 0.5 * (elapsed / 380.0).sin();
    if m.running {
        return (
            '►',
            Style::default()
                .fg(mix(AUTO, WHITE, pulse))
                .add_modifier(Modifier::BOLD),
        );
    }
    if m.gated {
        return ('◆', Style::default().fg(GATED).add_modifier(Modifier::BOLD));
    }
    if m.installed {
        return ('●', Style::default().fg(GOOD).add_modifier(Modifier::BOLD));
    }
    if m.removed_at.is_some() {
        // Was here once.
        return ('◍', Style::default().fg(mix(GOOD, FAINT, 0.45)));
    }
    if m.offered {
        // In the repositories and not installed: this is what update brings.
        return ('◉', Style::default().fg(AUTO));
    }
    match r.kind {
        ReleaseKind::Mainline => ('◈', Style::default().fg(if mine { AUTO } else { MUTED })),
        ReleaseKind::Candidate if r.inferred => ('⋄', Style::default().fg(FAINT)),
        ReleaseKind::Candidate => ('◇', Style::default().fg(GHOST)),
        ReleaseKind::Point => ('○', Style::default().fg(if mine { AUTO } else { MUTED })),
    }
}

fn chips(m: &VersionMarks) -> Vec<(String, Style)> {
    let chip = |text: &str, fg: Color| {
        (
            format!(" {text} "),
            Style::default().fg(Color::Black).bg(fg),
        )
    };
    let mut v = Vec::new();
    let gap = || (" ".to_string(), Style::default());
    if m.running {
        v.push(chip("► running", AUTO));
        v.push(gap());
    }
    if m.installed {
        v.push(chip("● installed", GOOD));
        v.push(gap());
    }
    if m.default_boot {
        v.push(chip("★ boots by default", STAR));
        v.push(gap());
    }
    if m.known_good {
        v.push(chip("✓ known-good", GOOD));
        v.push(gap());
    }
    if m.testing {
        v.push(chip("◐ testing", AUTO));
        v.push(gap());
    }
    if m.gated {
        v.push(chip("◆ gated", GATED));
        v.push(gap());
    }
    if m.vaulted {
        v.push(chip("▣ vaulted", MUTED));
        v.push(gap());
    }
    if let (Some(r), false) = (m.removed_at, m.installed) {
        v.push(chip(&format!("◍ removed {r}"), MUTED));
        v.push(gap());
    }
    if m.offered && !m.installed {
        v.push(chip("◉ offered by your repositories", AUTO));
        v.push(gap());
    }
    if v.is_empty() {
        v.push(("not on this machine".into(), Style::default().fg(FAINT)));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Lane, Upstream};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn view() -> TimelineView {
        let lane = Lane {
            series: "7.2".into(),
            start: Some(d("2026-08-17")),
            end: None,
            state: LaneState::Active,
            releases: vec![
                Release {
                    version: "7.2".into(),
                    date: d("2026-08-17"),
                    kind: ReleaseKind::Mainline,
                    inferred: false,
                },
                Release {
                    version: "7.2.6".into(),
                    date: d("2026-09-14"),
                    kind: ReleaseKind::Point,
                    inferred: false,
                },
                Release {
                    version: "7.2.7".into(),
                    date: d("2026-09-21"),
                    kind: ReleaseKind::Point,
                    inferred: false,
                },
            ],
            projections: vec![Projection {
                label: "7.2.8".into(),
                date: d("2026-09-28"),
                spread_days: 2,
                kind: ProjectionKind::NextPoint,
            }],
        };
        let mut machine = Machine {
            series: Some("7.2".into()),
            ..Default::default()
        };
        machine.marks.insert(
            "7.2.0".into(),
            VersionMarks {
                installed: true,
                running: true,
                current: true,
                default_boot: true,
                ..Default::default()
            },
        );
        machine.marks.insert(
            "7.2.6".into(),
            VersionMarks {
                offered: true,
                ..Default::default()
            },
        );
        let src = Source {
            component: "kernel".into(),
            upstream: None,
            machine,
            shapes: BTreeMap::new(),
            has_shapes: true,
            boots: false,
        };
        let mut v = TimelineView::new("kernel", vec![src], 1);
        v.set_upstream(
            "kernel",
            Upstream {
                lanes: vec![lane],
                ..Default::default()
            },
        );
        v
    }

    fn render(v: &mut TimelineView) -> String {
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        // Skip the intro so everything is drawn.
        v.born = Instant::now() - std::time::Duration::from_secs(5);
        for _ in 0..60 {
            v.step();
        }
        v.render(area, &mut buf, "kernel");
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn history_and_freezes_show_on_the_timeline() {
        let mut v = view();
        {
            let m = &mut v.sources[0].machine;
            m.marks.insert(
                "7.2.6".into(),
                VersionMarks {
                    removed_at: Some(d("2026-09-18")),
                    offered: true,
                    ..Default::default()
                },
            );
            m.history.push(crate::timeline::Change {
                at: "2026-09-18T10:00:00Z".parse().unwrap(),
                removed: true,
                version: "7.2.6".into(),
            });
            m.records.insert(
                "7.2.0".into(),
                crate::evidence::KernelRecord {
                    boots: 9,
                    hours: 427.0,
                    unclean: 2,
                    pstore: 0,
                    inferred: 8,
                },
            );
        }
        v.sources[0].boots = true;
        v.rebuild_rows();
        let screen = render(&mut v);
        assert!(
            screen.contains('✘'),
            "a kernel with unclean ends is flagged:\n{screen}"
        );
        assert!(
            screen.contains('▽'),
            "the removal is on the machine row:\n{screen}"
        );
        assert!(
            screen.contains("2 unclean ends"),
            "the card carries the record:\n{screen}"
        );
        assert!(
            screen.contains("inferred"),
            "and says what was inferred:\n{screen}"
        );
    }

    #[test]
    fn a_later_revert_is_flagged_on_the_release_it_undid() {
        use crate::timeline::{ChangeRef, RevertRef, Tier};
        let mut v = view();
        let change = ChangeRef {
            subject: "drm/amdgpu: enable the shiny thing".into(),
            driver: "amdgpu".into(),
            tier: Tier::Hardware,
            ids: vec!["111111111111".into()],
        };
        v.set_shape(
            "kernel",
            "7.2.6",
            Some(Shape {
                patches: 10,
                changes: vec![change],
                ..Default::default()
            }),
        );
        v.set_shape(
            "kernel",
            "7.2.7",
            Some(Shape {
                patches: 3,
                reverts_of: vec![RevertRef {
                    subject: "drm/amdgpu: enable the shiny thing".into(),
                    ids: vec![],
                }],
                ..Default::default()
            }),
        );
        v.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let screen = render(&mut v);
        assert!(screen.contains('↺'), "the release is flagged:\n{screen}");
        assert!(
            screen.contains("reverted later, in 7.2.7"),
            "and the card says so:\n{screen}"
        );
        let notes = v.release_notes("kernel", "7.2.6");
        assert!(
            notes.iter().any(|n| n.contains("reverted later")),
            "{notes:?}"
        );
    }

    /// Events on the same day are scrubbed in the order they happened, not
    /// in whatever order they were collected.
    #[test]
    fn same_day_events_keep_their_real_order() {
        let mut v = view();
        {
            let m = &mut v.sources[0].machine;
            let t = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
            // Collected out of order on purpose.
            m.history.push(crate::timeline::Change {
                at: t("2026-08-27T16:05:00Z"),
                removed: true,
                version: "7.1.2".into(),
            });
            m.history.push(crate::timeline::Change {
                at: t("2026-08-27T16:00:00Z"),
                removed: false,
                version: "7.2.0".into(),
            });
            m.boots.push(crate::timeline::BootSpan {
                boot_id: None,
                start: t("2026-08-27T08:00:00Z"),
                end: t("2026-08-27T15:00:00Z"),
                clean: false,
                kernel: None,
                kernel_inferred: false,
                pstore_hits: 0,
            });
        }
        v.sources[0].boots = true;
        v.rebuild_rows();
        let machine_row = v
            .rows
            .iter()
            .position(|r| matches!(r, RowKind::Machine { .. }))
            .unwrap();
        let order: Vec<&str> = v
            .items(machine_row)
            .iter()
            .map(|(_, r)| match r {
                ItemRef::Boot { .. } => "boot",
                ItemRef::Install { .. } => "install",
                ItemRef::Removal { .. } => "removal",
                _ => "other",
            })
            .collect();
        assert_eq!(order, vec!["boot", "install", "removal"]);
    }

    #[test]
    fn moving_between_rows_prefers_what_is_on_screen() {
        let mut v = view();
        let old = Lane {
            series: "7.1".into(),
            start: Some(d("2026-06-14")),
            end: Some(d("2026-09-02")),
            state: LaneState::Eol,
            releases: vec![
                Release {
                    version: "7.1".into(),
                    date: d("2026-06-14"),
                    kind: ReleaseKind::Mainline,
                    inferred: false,
                },
                Release {
                    version: "7.1.13".into(),
                    date: d("2026-09-02"),
                    kind: ReleaseKind::Point,
                    inferred: false,
                },
            ],
            projections: vec![],
        };
        let mut up = v.sources[0].upstream.clone().unwrap();
        up.lanes.push(old);
        v.set_upstream("kernel", up);
        render(&mut v);
        // From 7.2.7 (late September), straight down lands on 7.1.13, which
        // is on screen, not the June mainline.
        v.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        v.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        match v.selected() {
            Some(ItemRef::Release { lane, index, .. }) => {
                let r = &v.sources[0].upstream.as_ref().unwrap().lanes[lane].releases[index];
                assert_eq!(r.version, "7.1.13");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opens_on_the_running_version() {
        let v = view();
        assert!(
            matches!(v.selected(), Some(ItemRef::Release { index: 0, .. })),
            "{:?}",
            v.selected()
        );
    }

    #[test]
    fn draws_the_machine_on_the_timeline() {
        let mut v = view();
        let screen = render(&mut v);
        assert!(
            screen.contains('►'),
            "the running version is marked:\n{screen}"
        );
        assert!(
            screen.contains('★'),
            "the default boot entry is marked:\n{screen}"
        );
        assert!(
            screen.contains('◉'),
            "what the repositories offer is marked:\n{screen}"
        );
        assert!(
            screen.contains('◌'),
            "the next release is a ghost:\n{screen}"
        );
        assert!(
            screen.contains("7.2.8?"),
            "and labelled as a guess:\n{screen}"
        );
        assert!(screen.contains("today"));
        assert!(
            screen.contains("running"),
            "the card says what the selection is:\n{screen}"
        );
    }

    #[test]
    fn scrubbing_moves_through_releases_then_projections() {
        let mut v = view();
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        v.handle_key(key(KeyCode::Right));
        assert!(matches!(
            v.selected(),
            Some(ItemRef::Release { index: 1, .. })
        ));
        v.handle_key(key(KeyCode::End));
        assert!(matches!(v.selected(), Some(ItemRef::Projection { .. })));
        v.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(
            v.selected(),
            Some(ItemRef::Release { index: 0, .. })
        ));
    }

    #[test]
    fn zoom_keeps_the_selection_in_place() {
        let mut v = view();
        render(&mut v);
        let sel = day(v.selected_date().unwrap());
        let before = (sel - v.vp.target_center) / v.vp.target_scale;
        v.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        let after = (sel - v.vp.target_center) / v.vp.target_scale;
        assert!((before - after).abs() < 0.01, "{before} vs {after}");
        assert!(v.vp.target_scale < v.vp.scale);
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn the_buttons_are_clickable() {
        let mut v = view();
        render(&mut v);
        let (more, _) = *v.hits.iter().find(|(_, h)| matches!(h, Hit::More)).unwrap();
        v.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            more.x + 1,
            more.y,
        ));
        assert_eq!(v.info, 2);
        let (zoom_in, _) = *v
            .hits
            .iter()
            .find(|(_, h)| matches!(h, Hit::ZoomIn))
            .unwrap();
        let before = v.vp.target_scale;
        v.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            zoom_in.x,
            zoom_in.y,
        ));
        assert!(v.vp.target_scale < before);
    }

    #[test]
    fn the_wheel_zooms_and_dragging_pans() {
        let mut v = view();
        render(&mut v);
        let (x, y) = (v.vp.plot.x + v.vp.plot.width / 2, v.vp.plot.y + 4);
        let scale = v.vp.target_scale;
        v.handle_mouse(mouse(MouseEventKind::ScrollUp, x, y));
        assert!(v.vp.target_scale < scale, "wheel up zooms in");

        let center = v.vp.target_center;
        v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x - 10, y));
        v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x - 10, y));
        assert!(
            v.vp.target_center > center,
            "dragging left moves later dates into view"
        );
    }

    #[test]
    fn info_levels_are_bounded() {
        let mut v = view();
        for _ in 0..10 {
            v.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        }
        assert_eq!(v.info, MAX_INFO);
        for _ in 0..10 {
            v.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        }
        assert_eq!(v.info, 0);
    }

    #[test]
    fn richer_levels_ask_for_what_changed() {
        let mut v = view();
        render(&mut v);
        v.info = 2;
        let wanted = v.wanted_shapes();
        assert!(
            wanted
                .iter()
                .any(|(c, ver)| c == "kernel" && ver == "7.2.6"),
            "{wanted:?}"
        );
        v.mark_loading("kernel", "7.2.6");
        assert!(
            !v.wanted_shapes().iter().any(|(_, ver)| ver == "7.2.6"),
            "not asked for twice"
        );
    }
}
