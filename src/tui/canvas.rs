//! What the timeline and the boot history share: a viewport over time that
//! pans and zooms with easing, a calendar axis that goes from years down to
//! hours, the palette, and a few drawing primitives.
//!
//! Positions on the axis are days since the common era, as `f64`, in local
//! time: a date sits at its midnight, an instant at its fraction of the day.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

pub const GATED: Color = Color::Rgb(0xf5, 0x9e, 0x0b);
pub const BAD: Color = Color::Rgb(0xef, 0x44, 0x44);
pub const GOOD: Color = Color::Rgb(0x22, 0xc5, 0x5e);
pub const AUTO: Color = Color::Rgb(0x38, 0xbd, 0xf8);
pub const MUTED: Color = Color::Rgb(0x94, 0xa3, 0xb8);
pub const FAINT: Color = Color::Rgb(0x47, 0x55, 0x69);
pub const GHOST: Color = Color::Rgb(0x64, 0x74, 0x8b);
pub const ACCENT: Color = Color::Rgb(0xa7, 0x8b, 0xfa);
pub const TODAY: Color = Color::Rgb(0xf4, 0x72, 0xb6);
pub const STAR: Color = Color::Rgb(0xfa, 0xcc, 0x15);
pub const CURSOR_BG: Color = Color::Rgb(0x1e, 0x29, 0x3b);
pub const WHITE: Color = Color::Rgb(0xf8, 0xfa, 0xfc);

pub const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn day(d: NaiveDate) -> f64 {
    f64::from(d.num_days_from_ce())
}

/// Where an instant sits on the axis: its local day plus the fraction of it.
pub fn when(t: DateTime<Utc>) -> f64 {
    let local = t.with_timezone(&chrono::Local);
    day(local.date_naive()) + f64::from(local.num_seconds_from_midnight()) / 86_400.0
}

pub fn now_x() -> f64 {
    when(Utc::now())
}

pub fn date_of(x: f64) -> NaiveDate {
    NaiveDate::from_num_days_from_ce_opt(x.floor() as i32).unwrap_or_default()
}

pub fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

pub fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn mix(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let l = |x: u8, y: u8| {
                (f64::from(x) + (f64::from(y) - f64::from(x)) * t.clamp(0.0, 1.0)) as u8
            };
            Color::Rgb(l(r1, r2), l(g1, g2), l(b1, b2))
        }
        _ => b,
    }
}

/// Write `text` at (x, y), clipped to `clip`. Returns the column after it.
pub fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, style: Style) -> u16 {
    if y < clip.y || y >= clip.y + clip.height {
        return x;
    }
    let mut cx = x;
    for ch in text.chars() {
        if cx >= clip.x + clip.width {
            break;
        }
        if cx >= clip.x {
            buf[(cx, y)].set_char(ch).set_style(style);
        }
        cx += 1;
    }
    cx
}

/// Like [`put`], but shifted left as needed so the whole text stays inside
/// `clip` — for labels near the right edge.
pub fn put_within(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, style: Style) -> u16 {
    let w = text.chars().count() as u16;
    let x = x.min((clip.x + clip.width).saturating_sub(w)).max(clip.x);
    put(buf, clip, x, y, text, style)
}

pub fn draw_box(buf: &mut Buffer, r: Rect, color: Color) {
    if r.width < 2 || r.height < 2 {
        return;
    }
    let st = Style::default().fg(color);
    let (x1, y1) = (r.x + r.width - 1, r.y + r.height - 1);
    for x in r.x..=x1 {
        buf[(x, r.y)].set_char('─').set_style(st);
        buf[(x, y1)].set_char('─').set_style(st);
    }
    for y in r.y..=y1 {
        buf[(r.x, y)].set_char('│').set_style(st);
        buf[(x1, y)].set_char('│').set_style(st);
    }
    buf[(r.x, r.y)].set_char('╭').set_style(st);
    buf[(x1, r.y)].set_char('╮').set_style(st);
    buf[(r.x, y1)].set_char('╰').set_style(st);
    buf[(x1, y1)].set_char('╯').set_style(st);
}

fn add_months(d: NaiveDate, n: u32) -> NaiveDate {
    let total = d.year() * 12 + d.month0() as i32 + n as i32;
    NaiveDate::from_ymd_opt(total / 12, (total % 12) as u32 + 1, 1).unwrap_or(d)
}

/// How much time ten columns cover, for the zoom readout.
pub fn zoom_label(scale: f64) -> String {
    let per10 = scale * 10.0;
    if per10 < 1.0 {
        format!("{:.0}h/10col", (per10 * 24.0).max(1.0))
    } else if per10 < 14.0 {
        format!("{per10:.0}d/10col")
    } else if per10 < 60.0 {
        format!("{:.0}w/10col", per10 / 7.0)
    } else if per10 < 700.0 {
        format!("{:.0}mo/10col", per10 / 30.4)
    } else {
        format!("{:.0}y/10col", per10 / 365.0)
    }
}

/// A window onto time: the day at its centre and days per column, each
/// easing toward a target so every move is animated rather than jumped.
#[derive(Debug, Clone)]
pub struct Viewport {
    pub center: f64,
    pub target_center: f64,
    pub scale: f64,
    pub target_scale: f64,
    /// Where the plot was last drawn.
    pub plot: Rect,
    pub min_scale: f64,
    pub max_scale: f64,
}

impl Viewport {
    pub fn new(center: f64, scale: f64, min_scale: f64, max_scale: f64) -> Self {
        Viewport {
            center,
            target_center: center,
            scale,
            target_scale: scale,
            plot: Rect::default(),
            min_scale,
            max_scale,
        }
    }

    pub fn visible_days(&self) -> (f64, f64) {
        let half = f64::from(self.plot.width) / 2.0 * self.scale;
        (self.center - half, self.center + half)
    }

    /// The column showing `x`, if it is on screen.
    pub fn col(&self, x: f64) -> Option<u16> {
        let (lo, _) = self.visible_days();
        let c = ((x - lo) / self.scale).round();
        (c >= 0.0 && c < f64::from(self.plot.width)).then(|| self.plot.x + c as u16)
    }

    /// The position a column shows.
    pub fn at(&self, column: u16) -> f64 {
        let (lo, _) = self.visible_days();
        lo + f64::from(column.saturating_sub(self.plot.x)) * self.scale
    }

    pub fn settling(&self) -> bool {
        (self.center - self.target_center).abs() > 0.02 * self.scale.max(0.01)
            || (self.scale / self.target_scale - 1.0).abs() > 0.002
    }

    /// Advance the easing by one frame.
    pub fn step(&mut self) {
        let k = 0.28;
        self.center += (self.target_center - self.center) * k;
        self.scale *= (self.target_scale / self.scale).powf(k);
        if (self.center - self.target_center).abs() < 0.02 * self.scale.max(0.01) {
            self.center = self.target_center;
        }
        if (self.scale / self.target_scale - 1.0).abs() < 0.002 {
            self.scale = self.target_scale;
        }
    }

    /// Show `lo..hi`.
    pub fn frame(&mut self, lo: f64, hi: f64) {
        let width = f64::from(self.plot.width.max(40));
        self.target_center = (lo + hi) / 2.0;
        self.target_scale = ((hi - lo) / width).clamp(self.min_scale, self.max_scale);
    }

    /// Zoom by `factor`, keeping `anchor` where it is on screen.
    pub fn zoom(&mut self, factor: f64, anchor: f64) {
        let new_scale = (self.target_scale * factor).clamp(self.min_scale, self.max_scale);
        self.target_center =
            anchor - (anchor - self.target_center) * (new_scale / self.target_scale);
        self.target_scale = new_scale;
    }

    pub fn pan(&mut self, columns: f64) {
        self.target_center += columns * self.target_scale;
    }

    /// Bring `x` into view if it is near an edge or off screen.
    pub fn ensure_visible(&mut self, x: f64) {
        let half = f64::from(self.plot.width.max(20)) / 2.0 * self.target_scale;
        let margin = half * 0.8;
        if (x - self.target_center).abs() > margin {
            self.target_center = x - margin.copysign(x - self.target_center) * 0.5;
        }
    }

    /// A calendar axis on rows `y` (labels) and `y + 1` (ticks), from hours
    /// up to years, whichever keeps labels apart.
    pub fn draw_axis(&self, buf: &mut Buffer, y: u16) {
        let (lo, hi) = self.visible_days();
        let min_gap = 9.0;
        let units = [
            1.0 / 24.0,
            3.0 / 24.0,
            6.0 / 24.0,
            12.0 / 24.0,
            1.0,
            7.0,
            30.4,
            91.3,
            182.6,
            365.25,
            730.5,
        ];
        let unit = units
            .into_iter()
            .find(|u| u / self.scale >= min_gap)
            .unwrap_or(1461.0);
        let mut ticks: Vec<(f64, String, bool)> = Vec::new();
        if unit < 0.99 {
            // Hours: midnight is the major tick and carries the date.
            let mut x = (lo / unit).floor() * unit;
            while x <= hi {
                let d = date_of(x + 1e-6);
                let hour = ((x - x.floor()) * 24.0).round() as u32 % 24;
                let major = hour == 0;
                let text = if major {
                    d.format("%a %b %-d").to_string()
                } else {
                    format!("{hour:02}:00")
                };
                ticks.push((x, text, major));
                x += unit;
            }
        } else if unit < 7.5 {
            let step = unit as i64;
            let mut d = date_of(lo);
            if step == 7 {
                while d.weekday() != chrono::Weekday::Mon {
                    d += Duration::days(1);
                }
            }
            let hi_d = date_of(hi);
            while d <= hi_d {
                let major = d.day() <= step as u32;
                let text = if major || step == 7 {
                    d.format("%b %-d").to_string()
                } else {
                    d.format("%-d").to_string()
                };
                ticks.push((day(d), text, major));
                d += Duration::days(step);
            }
        } else {
            let months = (unit / 30.4).round().max(1.0) as u32;
            let lo_d = date_of(lo);
            let hi_d = date_of(hi);
            let mut d = NaiveDate::from_ymd_opt(lo_d.year(), lo_d.month(), 1).unwrap_or(lo_d);
            while d <= hi_d {
                if (d.month() - 1).is_multiple_of(months.min(12)) || months > 12 {
                    let major = d.month() == 1;
                    let text = if months >= 12 || major {
                        d.format("%Y").to_string()
                    } else if months >= 3 {
                        format!("Q{}", (d.month() - 1) / 3 + 1)
                    } else {
                        d.format("%b").to_string()
                    };
                    if months <= 12 || d.year() % (months / 12) as i32 == 0 {
                        ticks.push((day(d), text, major));
                    }
                }
                d = add_months(d, 1);
            }
        }
        let mut last_end = 0u16;
        for (x, text, major) in ticks {
            let Some(c) = self.col(x) else { continue };
            buf[(c, y + 1)]
                .set_char(if major { '┴' } else { '╵' })
                .set_fg(if major { MUTED } else { FAINT });
            if c >= last_end {
                let style = Style::default().fg(if major { WHITE } else { MUTED });
                last_end = put(buf, self.plot, c, y, &text, style) + 1;
            }
        }
        for c in self.plot.x..self.plot.x + self.plot.width {
            let cell = &mut buf[(c, y + 1)];
            if cell.symbol() == " " {
                cell.set_char('─').set_fg(FAINT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> Viewport {
        let mut v = Viewport::new(1000.0, 1.0, 1.0 / 48.0, 90.0);
        v.plot = Rect::new(10, 0, 100, 10);
        v
    }

    #[test]
    fn columns_and_positions_round_trip() {
        let v = vp();
        let c = v.col(1000.0).unwrap();
        assert_eq!(c, 60, "the centre is mid-plot");
        assert!((v.at(c) - 1000.0).abs() < 1e-9);
        assert!(v.col(2000.0).is_none(), "off screen is None");
    }

    #[test]
    fn zooming_keeps_the_anchor_in_place() {
        let mut v = vp();
        let before = (1010.0 - v.target_center) / v.target_scale;
        v.zoom(0.5, 1010.0);
        let after = (1010.0 - v.target_center) / v.target_scale;
        assert!((before - after).abs() < 1e-9);
        v.zoom(1e-6, 1010.0);
        assert!(
            (v.target_scale - 1.0 / 48.0).abs() < 1e-12,
            "clamped to half an hour a column"
        );
    }

    #[test]
    fn the_axis_reaches_down_to_hours() {
        let mut v = vp();
        v.scale = 1.0 / 96.0; // 15 minutes a column
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 3));
        v.draw_axis(&mut buf, 0);
        let row: String = (0..120).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(row.contains(":00"), "{row}");
    }
}
