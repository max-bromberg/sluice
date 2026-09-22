//! The boot history: this machine's own life, laid out in time.
//!
//! One lane per kernel, each boot a bar from start to end, and a boot that
//! ended without a shutdown marked where it stopped. Around them, what changed:
//! kernels installed and removed, updates run, versions marked good. Select a
//! boot and the card shows how it ended — and the last lines its journal holds,
//! which after a freeze are the only evidence there is.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::canvas::*;
use crate::evidence::KernelRecord;
use crate::state::UpdateRun;
use crate::timeline::{BootSpan, Change};

/// Distinct colours for kernels, none of them red: red means "ended badly".
const KERNEL_COLORS: [Color; 6] = [
    AUTO,
    GOOD,
    STAR,
    ACCENT,
    Color::Rgb(0x2d, 0xd4, 0xbf),
    Color::Rgb(0xfb, 0x92, 0x3c),
];

/// Something that happened to the machine besides booting.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Change(Change),
    Update(UpdateRun),
    MarkedGood { at: DateTime<Utc>, version: String },
}

impl Event {
    fn at(&self) -> DateTime<Utc> {
        match self {
            Event::Change(c) => c.at,
            Event::Update(u) => u.started,
            Event::MarkedGood { at, .. } => *at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Kernel(usize),
    Events,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Boot(usize),
    Event(usize),
}

pub enum Tail {
    Loading,
    Ready(Vec<String>),
}

pub struct BootsView {
    boots: Vec<BootSpan>,
    events: Vec<Event>,
    records: BTreeMap<String, KernelRecord>,
    /// Kernel versions with boots, newest first; `unknown` last if any.
    kernels: Vec<String>,
    rows: Vec<Row>,
    vp: Viewport,
    row: usize,
    item: usize,
    pub expanded: bool,
    pub tails: BTreeMap<String, Tail>,
    born: Instant,
    drag: Option<(u16, f64)>,
    row_y: Vec<(u16, usize)>,
    fitted: bool,
}

fn kernel_key(b: &BootSpan) -> String {
    b.version().unwrap_or_else(|| "unknown".into())
}

impl BootsView {
    pub fn new(
        boots: Vec<BootSpan>,
        events: Vec<Event>,
        records: BTreeMap<String, KernelRecord>,
    ) -> Self {
        let mut boots = boots;
        boots.sort_by_key(|b| b.start);
        let mut events = events;
        events.sort_by_key(|e| e.at());

        let mut kernels: Vec<String> = Vec::new();
        for b in &boots {
            let k = kernel_key(b);
            if !kernels.contains(&k) {
                kernels.push(k);
            }
        }
        kernels.sort_by(|a, b| match (a.as_str(), b.as_str()) {
            ("unknown", _) => std::cmp::Ordering::Greater,
            (_, "unknown") => std::cmp::Ordering::Less,
            _ => crate::version::Evr::parse(b).cmp(&crate::version::Evr::parse(a)),
        });
        let mut rows: Vec<Row> = (0..kernels.len()).map(Row::Kernel).collect();
        if !events.is_empty() {
            rows.push(Row::Events);
        }

        let mut v = BootsView {
            boots,
            events,
            records,
            kernels,
            rows,
            vp: Viewport::new(now_x(), 1.0, 1.0 / 48.0, 60.0),
            row: 0,
            item: 0,
            expanded: false,
            tails: BTreeMap::new(),
            born: Instant::now(),
            drag: None,
            row_y: Vec::new(),
            fitted: false,
        };
        v.select_latest_boot();
        v
    }

    fn color_of(&self, kernel: &str) -> Color {
        match self.kernels.iter().position(|k| k == kernel) {
            Some(_) if kernel == "unknown" => MUTED,
            Some(i) => KERNEL_COLORS[i % KERNEL_COLORS.len()],
            None => MUTED,
        }
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    fn items(&self, row: usize) -> Vec<(f64, Item)> {
        match self.rows.get(row) {
            Some(Row::Kernel(k)) => self
                .boots
                .iter()
                .enumerate()
                .filter(|(_, b)| kernel_key(b) == self.kernels[*k])
                .map(|(i, b)| (when(b.end), Item::Boot(i)))
                .collect(),
            Some(Row::Events) => self
                .events
                .iter()
                .enumerate()
                .map(|(i, e)| (when(e.at()), Item::Event(i)))
                .collect(),
            None => Vec::new(),
        }
    }

    fn selected(&self) -> Option<(f64, Item)> {
        self.items(self.row).get(self.item).copied()
    }

    fn select_item(&mut self, target: Item) {
        for row in 0..self.rows.len() {
            if let Some(i) = self.items(row).iter().position(|(_, it)| *it == target) {
                self.row = row;
                self.item = i;
                if let Some((x, _)) = self.selected() {
                    self.vp.ensure_visible(x);
                }
                return;
            }
        }
    }

    fn select_latest_boot(&mut self) {
        if let Some(i) = self.boots.len().checked_sub(1) {
            self.select_item(Item::Boot(i));
        }
    }

    /// The boot whose journal tail the card wants, if not already known.
    pub fn wanted_tail(&self) -> Option<String> {
        match self.selected() {
            Some((_, Item::Boot(i))) => {
                let id = self.boots[i].boot_id.clone()?;
                (!self.tails.contains_key(&id)).then_some(id)
            }
            _ => None,
        }
    }

    pub fn set_tail(&mut self, boot_id: &str, lines: Vec<String>) {
        self.tails.insert(boot_id.to_string(), Tail::Ready(lines));
    }

    pub fn mark_tail_loading(&mut self, boot_id: &str) {
        self.tails.insert(boot_id.to_string(), Tail::Loading);
    }

    pub fn animating(&self) -> bool {
        self.vp.settling() || self.born.elapsed().as_millis() < 900
    }

    pub fn step(&mut self) {
        self.vp.step();
    }

    fn fit_all(&mut self) {
        let lo = self.boots.first().map(|b| when(b.start));
        let hi = self.boots.last().map(|b| when(b.end));
        if let (Some(lo), Some(hi)) = (lo, hi) {
            let pad = ((hi - lo) * 0.03).max(0.5);
            self.vp.frame(lo - pad, hi.max(now_x()) + pad);
        }
    }

    fn unclean_boots(&self) -> Vec<usize> {
        (0..self.boots.len())
            .filter(|i| !self.boots[*i].clean)
            .collect()
    }

    /// Jump to the next (or previous) boot that did not shut down.
    fn next_unclean(&mut self, forward: bool) {
        let current = match self.selected() {
            Some((_, Item::Boot(i))) => self.boots[i].end,
            Some((_, Item::Event(i))) => self.events[i].at(),
            None => Utc::now(),
        };
        let unclean = self.unclean_boots();
        let pick = if forward {
            unclean.iter().find(|i| self.boots[**i].end > current)
        } else {
            unclean.iter().rev().find(|i| self.boots[**i].end < current)
        };
        if let Some(&i) = pick {
            self.select_item(Item::Boot(i));
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = f64::from(self.vp.plot.width.max(20)) / 3.0;
        match key.code {
            KeyCode::Left if shift => self.vp.pan(-page),
            KeyCode::Right if shift => self.vp.pan(page),
            KeyCode::Char('H') => self.vp.pan(-page),
            KeyCode::Char('L') => self.vp.pan(page),
            KeyCode::Left | KeyCode::Char('h') => self.step_item(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_item(1),
            KeyCode::Up => self.step_row(-1),
            KeyCode::Down => self.step_row(1),
            KeyCode::Home => {
                self.item = 0;
                self.follow();
            }
            KeyCode::End => {
                self.item = self.items(self.row).len().saturating_sub(1);
                self.follow();
            }
            KeyCode::Char('n') => self.next_unclean(true),
            KeyCode::Char('N') => self.next_unclean(false),
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom(1.0 / 1.6, None),
            KeyCode::Char('-') | KeyCode::Char('_') => self.zoom(1.6, None),
            KeyCode::Enter | KeyCode::Char(']') | KeyCode::Char('[') => {
                self.expanded = !self.expanded
            }
            KeyCode::Char('f') => self.fit_all(),
            KeyCode::Char('t') => self.vp.target_center = now_x(),
            KeyCode::Char('c') => self.select_latest_boot(),
            _ => return false,
        }
        true
    }

    fn follow(&mut self) {
        if let Some((x, _)) = self.selected() {
            self.vp.ensure_visible(x);
        }
    }

    fn zoom(&mut self, factor: f64, anchor: Option<f64>) {
        let a = anchor
            .or_else(|| self.selected().map(|(x, _)| x))
            .unwrap_or(self.vp.target_center);
        self.vp.zoom(factor, a);
    }

    fn step_item(&mut self, delta: isize) {
        let n = self.items(self.row).len();
        if n == 0 {
            return;
        }
        self.item = (self.item as isize + delta).clamp(0, n as isize - 1) as usize;
        self.follow();
    }

    fn step_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let from = self.selected().map(|(x, _)| x);
        self.row = (self.row as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        let items = self.items(self.row);
        let (lo, hi) = self.vp.visible_days();
        let at = from.unwrap_or(self.vp.center);
        let nearest = |only_visible: bool| {
            items
                .iter()
                .enumerate()
                .filter(|(_, (x, _))| !only_visible || (lo..=hi).contains(x))
                .min_by(|(_, (a, _)), (_, (b, _))| (a - at).abs().total_cmp(&(b - at).abs()))
                .map(|(i, _)| i)
        };
        self.item = nearest(true).or_else(|| nearest(false)).unwrap_or(0);
        self.follow();
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> bool {
        let at = self.vp.at(m.column);
        match m.kind {
            MouseEventKind::ScrollUp => self.zoom(1.0 / 1.25, Some(at)),
            MouseEventKind::ScrollDown => self.zoom(1.25, Some(at)),
            MouseEventKind::Down(MouseButton::Left) => {
                let p = self.vp.plot;
                if m.column < p.x || m.column >= p.x + p.width {
                    return false;
                }
                self.drag = Some((m.column, self.vp.target_center));
                if let Some(&(_, row)) = self.row_y.iter().find(|(y, _)| *y == m.row) {
                    let items = self.items(row);
                    // A click inside a boot's bar selects it.
                    let hit = items.iter().position(|(_, it)| match it {
                        Item::Boot(i) => {
                            let b = &self.boots[*i];
                            (when(b.start)..=when(b.end) + self.vp.scale).contains(&at)
                        }
                        Item::Event(_) => false,
                    });
                    let near = || {
                        items
                            .iter()
                            .enumerate()
                            .min_by(|(_, (a, _)), (_, (b, _))| {
                                (a - at).abs().total_cmp(&(b - at).abs())
                            })
                            .filter(|(_, (x, _))| (x - at).abs() <= 2.0 * self.vp.scale)
                            .map(|(i, _)| i)
                    };
                    if let Some(i) = hit.or_else(near) {
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

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.row_y.clear();
        let elapsed = self.born.elapsed().as_millis() as f64;
        draw_box(buf, area, FAINT);
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        if inner.width < 40 || inner.height < 12 {
            put(
                buf,
                inner,
                inner.x,
                inner.y,
                "window too small for the boot history",
                Style::default().fg(MUTED),
            );
            return;
        }

        // Title and totals.
        let hours: f64 = self
            .boots
            .iter()
            .map(|b| (b.end - b.start).num_minutes() as f64 / 60.0)
            .sum();
        let unclean = self.unclean_boots().len();
        let mut x = put(
            buf,
            inner,
            inner.x,
            inner.y,
            " this machine ",
            Style::default()
                .fg(WHITE)
                .bg(Color::Rgb(0x33, 0x41, 0x55))
                .add_modifier(Modifier::BOLD),
        );
        x = put(
            buf,
            inner,
            x + 1,
            inner.y,
            "boot history",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        );
        let since = self
            .boots
            .first()
            .map(|b| {
                b.start
                    .with_timezone(&Local)
                    .format(" since %Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default();
        put(
            buf,
            inner,
            x + 2,
            inner.y,
            &format!(
                "{} boots · {hours:.0} h · {unclean} ended without a shutdown{since}",
                self.boots.len()
            ),
            Style::default().fg(GHOST),
        );
        let zoom = format!("{}  ", zoom_label(self.vp.scale));
        put(
            buf,
            inner,
            (inner.x + inner.width).saturating_sub(zoom.chars().count() as u16),
            inner.y,
            &zoom,
            Style::default().fg(MUTED),
        );

        // Per-kernel summary: how each has behaved here, at a glance.
        let label_w: u16 = self
            .kernels
            .iter()
            .map(|k| k.chars().count() as u16 + 3)
            .chain(std::iter::once(9))
            .max()
            .unwrap_or(9)
            .min(20);
        let mut y = inner.y + 2;
        let max_hours = self.records.values().map(|r| r.hours).fold(1.0, f64::max);
        let worst = self
            .records
            .values()
            .filter_map(KernelRecord::rate)
            .fold(0.0, f64::max);
        for k in self.kernels.iter().take(6) {
            let color = self.color_of(k);
            put(buf, inner, inner.x + 1, y, "■", Style::default().fg(color));
            put(buf, inner, inner.x + 3, y, k, Style::default().fg(WHITE));
            if let Some(r) = self.records.get(k) {
                let bar_w = 24usize;
                let filled = ((r.hours / max_hours) * bar_w as f64)
                    .round()
                    .clamp(1.0, bar_w as f64) as usize;
                let bar: String = "█".repeat(filled) + &"░".repeat(bar_w - filled);
                let bx = put(
                    buf,
                    inner,
                    inner.x + label_w + 1,
                    y,
                    &bar,
                    Style::default().fg(color),
                );
                let rate_color = match r.rate() {
                    Some(rate) if r.unclean > 0 && rate >= worst => BAD,
                    Some(_) if r.unclean > 0 => GATED,
                    _ => GOOD,
                };
                put(
                    buf,
                    inner,
                    bx + 2,
                    y,
                    &r.summary(),
                    Style::default().fg(rate_color),
                );
            }
            y += 1;
        }
        if self.kernels.len() > 6 {
            put(
                buf,
                inner,
                inner.x + 3,
                y,
                &format!("… {} more", self.kernels.len() - 6),
                Style::default().fg(GHOST),
            );
            y += 1;
        }

        // The canvas.
        let card_h: u16 = if self.expanded { 16 } else { 10 };
        let axis_y = y + 1;
        let rows_top = axis_y + 2;
        let rows_bottom = (inner.y + inner.height).saturating_sub(card_h.min(inner.height / 2));
        self.vp.plot = Rect {
            x: inner.x + label_w,
            y: axis_y,
            width: inner.width.saturating_sub(label_w + 1),
            height: rows_bottom.saturating_sub(axis_y),
        };
        if !self.fitted {
            self.fitted = true;
            // Open on the last few weeks, not the whole journal.
            let now = now_x();
            self.vp.frame(now - 42.0, now + 2.0);
            self.vp.scale = self.vp.target_scale * 1.6;
            self.vp.center = self.vp.target_center;
        }
        self.vp.draw_axis(buf, axis_y);

        let reveal = ease_out(elapsed / 700.0);
        let reveal_col = self.vp.plot.x + (f64::from(self.vp.plot.width) * reveal) as u16;
        let mut y = rows_top;
        for ri in 0..self.rows.len() {
            if y + 1 > rows_bottom {
                break;
            }
            self.row_y.push((y, ri));
            let selected_row = ri == self.row;
            if selected_row {
                put(buf, inner, inner.x, y, "▸", Style::default().fg(ACCENT));
            }
            match self.rows[ri] {
                Row::Kernel(k) => {
                    let name = self.kernels[k].clone();
                    let mut st = Style::default().fg(self.color_of(&name));
                    if selected_row {
                        st = st.add_modifier(Modifier::UNDERLINED);
                    }
                    put(buf, inner, inner.x + 1, y, &name, st);
                    self.draw_kernel_row(buf, &name, y, reveal_col, elapsed);
                }
                Row::Events => {
                    let mut st = Style::default().fg(STAR).add_modifier(Modifier::ITALIC);
                    if selected_row {
                        st = st.add_modifier(Modifier::UNDERLINED);
                    }
                    put(buf, inner, inner.x + 1, y, "changes", st);
                    self.draw_events(buf, y, reveal_col);
                }
            }
            y += 2;
        }

        // Now, and the selection.
        if let Some(c) = self.vp.col(now_x()) {
            put(
                buf,
                self.vp.plot,
                c.saturating_sub(2),
                axis_y,
                " now ",
                Style::default().fg(TODAY).add_modifier(Modifier::BOLD),
            );
            for yy in axis_y + 1..rows_bottom {
                let cell = &mut buf[(c, yy)];
                if cell.symbol() == " " {
                    cell.set_char('│').set_fg(mix(TODAY, CURSOR_BG, 0.45));
                }
            }
        }
        if let Some((x, item)) = self.selected() {
            if let Some(c) = self.vp.col(x) {
                for yy in axis_y + 2..rows_bottom {
                    buf[(c, yy)].set_bg(CURSOR_BG);
                }
                let label = match item {
                    Item::Boot(i) => self.boots[i]
                        .end
                        .with_timezone(&Local)
                        .format(" %Y-%m-%d %H:%M ")
                        .to_string(),
                    Item::Event(i) => self.events[i]
                        .at()
                        .with_timezone(&Local)
                        .format(" %Y-%m-%d %H:%M ")
                        .to_string(),
                };
                let lx = c.saturating_sub(label.chars().count() as u16 / 2);
                put_within(
                    buf,
                    self.vp.plot,
                    lx,
                    axis_y + 1,
                    &label,
                    Style::default().fg(Color::Black).bg(ACCENT),
                );
            }
        }
        if reveal < 0.999 {
            for yy in rows_top..rows_bottom {
                if reveal_col < self.vp.plot.x + self.vp.plot.width {
                    buf[(reveal_col, yy)].set_char('▏').set_fg(ACCENT);
                }
            }
        }

        let card = Rect {
            x: inner.x,
            y: rows_bottom,
            width: inner.width,
            height: (inner.y + inner.height).saturating_sub(rows_bottom),
        };
        self.draw_card(buf, card, elapsed);
    }

    fn draw_kernel_row(
        &self,
        buf: &mut Buffer,
        kernel: &str,
        y: u16,
        reveal_col: u16,
        elapsed: f64,
    ) {
        let color = self.color_of(kernel);
        let selected = match self.selected() {
            Some((_, Item::Boot(i))) => Some(i),
            _ => None,
        };
        let (lo, hi) = self.vp.visible_days();
        // A kernel whose boots are all out of view says where they are.
        let span = self.boots.iter().filter(|b| kernel_key(b) == kernel).fold(
            None::<(f64, f64, DateTime<Utc>, DateTime<Utc>)>,
            |acc, b| {
                let (s, e) = (when(b.start), when(b.end));
                Some(match acc {
                    None => (s, e, b.start, b.end),
                    Some((s0, e0, t0, t1)) => (
                        s0.min(s),
                        e0.max(e),
                        if s < s0 { b.start } else { t0 },
                        if e > e0 { b.end } else { t1 },
                    ),
                })
            },
        );
        if let Some((first, last, t_first, t_last)) = span {
            let style = Style::default().fg(mix(color, FAINT, 0.4));
            if last < lo {
                put(
                    buf,
                    self.vp.plot,
                    self.vp.plot.x,
                    y,
                    &format!(
                        "◂ last boot {}",
                        t_last.with_timezone(&Local).format("%Y-%m-%d")
                    ),
                    style,
                );
            } else if first > hi {
                let text = format!(
                    "first boot {} ▸",
                    t_first.with_timezone(&Local).format("%Y-%m-%d")
                );
                put_within(
                    buf,
                    self.vp.plot,
                    self.vp.plot.x + self.vp.plot.width,
                    y,
                    &text,
                    style,
                );
            }
        }
        for (i, b) in self.boots.iter().enumerate() {
            if kernel_key(b) != kernel {
                continue;
            }
            let (from, to) = (when(b.start), when(b.end));
            if to < lo || from > hi {
                continue;
            }
            let c0 = self.vp.col(from.max(lo)).unwrap_or(self.vp.plot.x);
            let c1 = self
                .vp
                .col(to.min(hi))
                .unwrap_or(self.vp.plot.x + self.vp.plot.width - 1);
            let is_selected = selected == Some(i);
            let inferred = b.kernel_inferred;
            let fill = if is_selected {
                '█'
            } else if inferred {
                '▆'
            } else {
                '▇'
            };
            let st = Style::default().fg(if is_selected {
                mix(color, WHITE, 0.35)
            } else if inferred {
                mix(color, FAINT, 0.4)
            } else {
                color
            });
            for c in c0..=c1 {
                if c >= reveal_col {
                    break;
                }
                buf[(c, y)].set_char(fill).set_style(st);
            }
            // Where it ended.
            let current = i + 1 == self.boots.len() && b.clean;
            if let Some(c) = self.vp.col(to) {
                if c < reveal_col {
                    if !b.clean {
                        let end_color = if b.pstore_hits > 0 { WHITE } else { BAD };
                        buf[(c, y)]
                            .set_char('✘')
                            .set_style(Style::default().fg(end_color).add_modifier(Modifier::BOLD));
                    } else if current {
                        let pulse = 0.5 + 0.5 * (elapsed / 380.0).sin();
                        buf[(c, y)].set_char('►').set_fg(mix(AUTO, WHITE, pulse));
                    } else if self.vp.scale < 0.2 {
                        buf[(c, y)].set_char('▏').set_fg(mix(color, WHITE, 0.5));
                    }
                }
            }
            // Duration under a long enough bar.
            if c1 > c0 + 6 {
                let hours = (b.end - b.start).num_minutes() as f64 / 60.0;
                let text = if hours >= 48.0 {
                    format!("{:.0}d", hours / 24.0)
                } else {
                    format!("{hours:.0}h")
                };
                put(
                    buf,
                    self.vp.plot,
                    c0,
                    y + 1,
                    &text,
                    Style::default().fg(FAINT),
                );
            }
        }
    }

    fn draw_events(&self, buf: &mut Buffer, y: u16, reveal_col: u16) {
        // Events that land in one column are shown together: the most
        // important glyph, and all their labels.
        let mut columns: BTreeMap<u16, Vec<&Event>> = BTreeMap::new();
        for e in &self.events {
            if let Some(c) = self.vp.col(when(e.at())) {
                if c < reveal_col {
                    columns.entry(c).or_default().push(e);
                }
            }
        }
        let rank = |e: &Event| match e {
            Event::Update(u) if !u.ok => 5,
            Event::MarkedGood { .. } => 4,
            Event::Change(c) if !c.removed => 3,
            Event::Change(_) => 2,
            Event::Update(_) => 1,
        };
        let mut occupied: Vec<(u16, u16)> = Vec::new();
        for (c, events) in columns {
            let top = events.iter().max_by_key(|e| rank(e)).copied();
            let Some(top) = top else { continue };
            let (glyph, color) = match top {
                Event::Change(ch) if ch.removed => ('▽', mix(BAD, FAINT, 0.4)),
                Event::Change(_) => ('▼', GOOD),
                Event::Update(u) if u.ok => ('◆', AUTO),
                Event::Update(_) => ('✘', BAD),
                Event::MarkedGood { .. } => ('✓', GOOD),
            };
            buf[(c, y)].set_char(glyph).set_fg(color);
            let label: Vec<String> = events
                .iter()
                .map(|e| match e {
                    Event::Change(ch) if ch.removed => format!("−{}", ch.version),
                    Event::Change(ch) => format!("+{}", ch.version),
                    Event::Update(u) if u.ok => "update".into(),
                    Event::Update(_) => "update failed".into(),
                    Event::MarkedGood { version, .. } => format!("good {version}"),
                })
                .collect();
            let label = label.join(" ");
            let x1 = c + label.chars().count() as u16;
            if !occupied.iter().any(|(a, b)| c <= *b && *a <= x1) {
                let end = put_within(
                    buf,
                    self.vp.plot,
                    c,
                    y + 1,
                    &label,
                    Style::default().fg(color),
                );
                occupied.push((end.saturating_sub(label.chars().count() as u16), end));
            }
        }
    }

    fn draw_card(&self, buf: &mut Buffer, area: Rect, elapsed: f64) {
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
        let dim = Style::default().fg(MUTED);
        let spinner = SPINNER[(elapsed / 90.0) as usize % SPINNER.len()];
        let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
        let fmt = |t: DateTime<Utc>| {
            t.with_timezone(&Local)
                .format("%a %Y-%m-%d %H:%M")
                .to_string()
        };

        match self.selected() {
            None => lines.push(vec![("no boots recorded in the journal".into(), dim)]),
            Some((_, Item::Boot(i))) => {
                let b = &self.boots[i];
                let hours = (b.end - b.start).num_minutes() as f64 / 60.0;
                let kernel = kernel_key(b);
                let color = self.color_of(&kernel);
                let current = i + 1 == self.boots.len() && b.clean;
                lines.push(vec![
                    ("■ ".into(), Style::default().fg(color)),
                    (
                        if current {
                            "this boot".into()
                        } else {
                            "a boot".to_string()
                        },
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (
                        format!(
                            "   {} → {} · {}",
                            fmt(b.start),
                            if current {
                                "now".into()
                            } else {
                                b.end.with_timezone(&Local).format("%H:%M").to_string()
                            },
                            if hours >= 48.0 {
                                format!("{:.1} days", hours / 24.0)
                            } else {
                                format!("{hours:.1} h")
                            }
                        ),
                        dim,
                    ),
                ]);
                let kernel_text = match &b.kernel {
                    Some(k) if b.kernel_inferred => {
                        format!("kernel {k} (inferred from install history)")
                    }
                    Some(k) => format!("kernel {k}"),
                    None => "kernel unknown (a root `sluice check` records it)".into(),
                };
                let ending = if current {
                    ("still running".to_string(), Style::default().fg(AUTO))
                } else if b.clean {
                    (
                        "ended with a clean shutdown".to_string(),
                        Style::default().fg(GOOD),
                    )
                } else {
                    (
                        "ended WITHOUT a shutdown — a freeze, a crash or a power loss".to_string(),
                        Style::default().fg(BAD).add_modifier(Modifier::BOLD),
                    )
                };
                lines.push(vec![
                    (kernel_text, Style::default().fg(color)),
                    ("  ·  ".into(), dim),
                    ending,
                ]);
                if b.pstore_hits > 0 {
                    lines.push(vec![(
                        format!(
                            "{} crash record(s) in /var/lib/systemd/pstore from this boot",
                            b.pstore_hits
                        ),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    )]);
                }
                if let Some(r) = self.records.get(&kernel) {
                    lines.push(vec![
                        ("this kernel here: ".into(), Style::default().fg(STAR)),
                        (r.summary(), dim),
                    ]);
                }
                lines.push(vec![(
                    if b.clean {
                        "how its journal ends:"
                    } else {
                        "the last lines before it stopped:"
                    }
                    .to_string(),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                )]);
                match b.boot_id.as_ref().and_then(|id| self.tails.get(id)) {
                    Some(Tail::Ready(tail)) if tail.is_empty() => {
                        lines.push(vec![(
                            "  (nothing readable — the journal may have rotated it away)".into(),
                            dim,
                        )]);
                    }
                    Some(Tail::Ready(tail)) => {
                        let show = if self.expanded { 12 } else { 5 };
                        for l in tail.iter().rev().take(show).rev() {
                            lines.push(vec![
                                ("  │ ".into(), Style::default().fg(FAINT)),
                                (l.clone(), Style::default().fg(WHITE)),
                            ]);
                        }
                    }
                    _ => lines.push(vec![(format!("  {spinner} reading the journal…"), dim)]),
                }
            }
            Some((_, Item::Event(i))) => match &self.events[i] {
                Event::Change(c) => lines.push(vec![
                    (
                        if c.removed { "▽ " } else { "▼ " }.into(),
                        Style::default().fg(if c.removed { BAD } else { GOOD }),
                    ),
                    (
                        format!("kernel {}", c.version),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (
                        format!(
                            "   {} {}",
                            if c.removed { "removed" } else { "installed" },
                            fmt(c.at)
                        ),
                        dim,
                    ),
                ]),
                Event::MarkedGood { at, version } => lines.push(vec![
                    ("✓ ".into(), Style::default().fg(GOOD)),
                    (
                        format!("marked {version} known-good"),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    (format!("   {}", fmt(*at)), dim),
                ]),
                Event::Update(u) => {
                    lines.push(vec![
                        (
                            if u.ok { "◆ " } else { "✘ " }.into(),
                            Style::default().fg(if u.ok { AUTO } else { BAD }),
                        ),
                        (
                            if u.ok {
                                "update".to_string()
                            } else {
                                "update FAILED".to_string()
                            },
                            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                        ),
                        (format!("   {}", fmt(u.started)), dim),
                    ]);
                    if let Some(s) = &u.summary {
                        lines.push(vec![(s.clone(), dim)]);
                    }
                    if !u.applied.is_empty() {
                        lines.push(vec![(
                            format!("applied {}", u.applied.join(", ")),
                            Style::default().fg(GOOD),
                        )]);
                    }
                    for l in u.error.as_deref().unwrap_or("").lines().take(8) {
                        lines.push(vec![(l.trim().to_string(), Style::default().fg(BAD))]);
                    }
                }
            },
        }

        let hint = "←→ step  ↑↓ lane  n/N next/previous unclean end  enter more lines  +/−/wheel zoom  drag pan  c this boot  f fit  t now";
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

/// The last lines of one boot's journal. Unprivileged this is the user's
/// own journal; with root or the systemd-journal group, the system's too.
pub fn journal_tail(journalctl: &str, boot_id: &str, lines: usize) -> Vec<String> {
    std::process::Command::new(journalctl)
        .args([
            "-b",
            boot_id,
            "-n",
            &lines.to_string(),
            "-o",
            "short",
            "--no-pager",
            "-q",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.to_string())
                .filter(|l| !l.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn boot(id: &str, start: &str, end: &str, kernel: &str, clean: bool) -> BootSpan {
        BootSpan {
            boot_id: Some(id.into()),
            start: t(start),
            end: t(end),
            clean,
            kernel: Some(kernel.into()),
            kernel_inferred: false,
            pstore_hits: 0,
        }
    }

    fn view() -> BootsView {
        let boots = vec![
            boot(
                "a",
                "2026-09-01T08:00:00Z",
                "2026-09-01T20:00:00Z",
                "7.1.2-1-default",
                false,
            ),
            boot(
                "b",
                "2026-09-02T08:00:00Z",
                "2026-09-02T20:00:00Z",
                "7.2.0-1-default",
                true,
            ),
            boot(
                "c",
                "2026-09-03T08:00:00Z",
                "2026-09-04T02:00:00Z",
                "7.2.0-1-default",
                false,
            ),
            boot(
                "d",
                "2026-09-05T08:00:00Z",
                "2026-09-05T20:00:00Z",
                "7.2.0-1-default",
                true,
            ),
        ];
        let events = vec![Event::Change(Change {
            at: t("2026-09-02T07:00:00Z"),
            removed: false,
            version: "7.2.0".into(),
        })];
        BootsView::new(boots, events, BTreeMap::new())
    }

    fn render(v: &mut BootsView) -> String {
        let area = Rect::new(0, 0, 130, 40);
        let mut buf = Buffer::empty(area);
        v.born = Instant::now() - std::time::Duration::from_secs(5);
        v.render(area, &mut buf);
        v.fit_all();
        for _ in 0..80 {
            v.step();
        }
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
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
    fn one_lane_per_kernel_newest_first() {
        let v = view();
        assert_eq!(v.kernels, vec!["7.2.0", "7.1.2"]);
        assert!(
            matches!(v.selected(), Some((_, Item::Boot(3)))),
            "opens on the latest boot"
        );
    }

    #[test]
    fn n_walks_the_unclean_ends_in_order() {
        let mut v = view();
        v.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
        assert!(matches!(v.selected(), Some((_, Item::Boot(2)))));
        v.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
        assert!(
            matches!(v.selected(), Some((_, Item::Boot(0)))),
            "across kernels"
        );
        v.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(v.selected(), Some((_, Item::Boot(2)))));
    }

    #[test]
    fn draws_boots_freezes_and_the_card() {
        let mut v = view();
        v.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
        v.set_tail("c", vec!["kwin_wayland[2210]: something ordinary".into()]);
        let screen = render(&mut v);
        assert!(screen.contains('✘'), "an unclean end is marked:\n{screen}");
        assert!(screen.contains("WITHOUT a shutdown"), "{screen}");
        assert!(
            screen.contains("something ordinary"),
            "the journal tail is shown:\n{screen}"
        );
        assert!(
            screen.contains("7.1.2"),
            "each kernel has a lane:\n{screen}"
        );
        assert!(
            screen.contains('▼'),
            "the install is on the changes row:\n{screen}"
        );
    }

    #[test]
    fn the_card_asks_for_the_tail_once() {
        let mut v = view();
        assert_eq!(v.wanted_tail().as_deref(), Some("d"));
        v.mark_tail_loading("d");
        assert_eq!(v.wanted_tail(), None);
    }
}
