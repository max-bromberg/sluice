//! Drawing the dashboard.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
};
use ratatui::Frame;

use super::state::{Dashboard, Modal, Tab};
use crate::app::ComponentStatus;
use crate::policy::Decision;
use crate::state::Lifecycle;
use crate::version::Evr;

// A small palette, so the meaning of a colour is defined in one place:
// amber is "your decision is needed", red is "something is wrong",
// green is "vouched for", cyan is "will happen automatically".
const GATED: Color = Color::Rgb(0xf5, 0x9e, 0x0b);
const BAD: Color = Color::Rgb(0xef, 0x44, 0x44);
const GOOD: Color = Color::Rgb(0x22, 0xc5, 0x5e);
const AUTO: Color = Color::Rgb(0x38, 0xbd, 0xf8);
const MUTED: Color = Color::Rgb(0x94, 0xa3, 0xb8);
const ACCENT: Color = Color::Rgb(0xa7, 0x8b, 0xfa);

pub fn draw(f: &mut Frame, d: &mut Dashboard) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0], d);

    if d.tab == Tab::Lineage {
        // The timeline is the whole canvas; j/k still switch what it shows.
        d.list_area = Rect::default();
        let name = d.selected_name().unwrap_or_default();
        let area = chunks[1];
        match d.active_timeline() {
            Some(tl) => tl.render(area, f.buffer_mut(), &name),
            None => f.render_widget(
                Paragraph::new("select a component to see its timeline")
                    .style(Style::default().fg(MUTED))
                    .block(bordered("Timeline")),
                area,
            ),
        }
    } else {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(30)])
            .split(chunks[1]);
        draw_components(f, body[0], d);
        draw_detail(f, body[1], d);
    }
    draw_footer(f, chunks[2], d);

    match d.modal.clone() {
        Modal::Confirm(action) => draw_confirm(f, &action),
        Modal::Help => draw_help(f),
        Modal::Output { title, body, ok } => draw_output(f, &title, &body, ok),
        Modal::None => {}
    }
}

fn draw_header(f: &mut Frame, area: Rect, d: &mut Dashboard) {
    let mut spans = vec![
        Span::styled(
            "sluice",
            Style::default().fg(AUTO).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];

    if d.app.runner.dry_run() {
        spans.push(Span::styled(
            " DRY RUN ",
            Style::default()
                .bg(GATED)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }

    let chips = attention(d);
    if chips.is_empty() && d.status.is_some() {
        spans.push(Span::styled("✓ all clear", Style::default().fg(GOOD)));
    }
    for (i, (text, color)) in chips.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(loading) = &d.loading {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(loading.clone(), Style::default().fg(MUTED)));
    }

    let source = d
        .app
        .config
        .source
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "built-in defaults".into());
    // Keep the end of a long path, which is the informative part.
    let room = usize::from(area.width.saturating_sub(4));
    let source = if source.chars().count() > room {
        let tail: String = source.chars().rev().take(room.saturating_sub(1)).collect();
        format!("…{}", tail.chars().rev().collect::<String>())
    } else {
        source
    };

    let tabs = Tabs::new(Tab::ALL.iter().map(|t| t.title()).collect::<Vec<_>>())
        .select(d.tab.index())
        .highlight_style(Style::default().fg(AUTO).add_modifier(Modifier::BOLD))
        .divider(Span::styled("│", Style::default().fg(MUTED)));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Line::from(spans))
        .title_bottom(Line::from(Span::styled(source, Style::default().fg(MUTED))).right_aligned());

    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(tabs, inner);
    d.tabs_area = inner;
}

/// Everything that deserves a glance, as short chips for the header.
fn attention(d: &Dashboard) -> Vec<(String, Color)> {
    let mut out = Vec::new();
    let Some(status) = &d.status else {
        return out;
    };
    let gated: Vec<&str> = status
        .components
        .iter()
        .filter(|c| c.eval.gated.is_some())
        .map(|c| c.eval.component.as_str())
        .collect();
    if !gated.is_empty() {
        out.push((format!("◆ {} gated", gated.join(", ")), GATED));
    }
    if status.components.iter().any(|c| c.boot_drift.is_some()) {
        out.push(("⚠ boot default moved".into(), BAD));
    }
    for c in &status.components {
        if c.eval.series_withdrawn {
            out.push((format!("! {} series withdrawn", c.eval.component), GATED));
        }
        if c.upstream_eol_days.is_some() {
            out.push((format!("! {} EOL upstream", c.eval.component), BAD));
        }
    }
    if let Some(esp) = status.esp.as_ref().filter(|_| status.esp_low) {
        out.push((format!("⚠ ESP {} MB free", esp.free_mb), GATED));
    }
    let unprotected: Vec<&str> = status
        .components
        .iter()
        .filter(|c| c.eval.policy != crate::config::Policy::Follow && c.known_good.is_none())
        .map(|c| c.eval.component.as_str())
        .collect();
    if !unprotected.is_empty() {
        out.push((
            format!("no rollback target: {}", unprotected.join(", ")),
            MUTED,
        ));
    }
    out
}

fn draw_components(f: &mut Frame, area: Rect, d: &mut Dashboard) {
    let Some(status) = &d.status else {
        f.render_widget(
            Paragraph::new("loading…")
                .style(Style::default().fg(MUTED))
                .block(bordered("Components")),
            area,
        );
        return;
    };

    let mut items: Vec<ListItem> = status
        .components
        .iter()
        .map(|c| {
            let (marker, color) = decision_marker(c);
            let version = c
                .eval
                .installed
                .as_ref()
                .map_or_else(|| "—".into(), |v| v.version.clone());

            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        c.eval.component.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(version, Style::default().fg(MUTED)),
                    Span::raw(" "),
                    Span::styled(
                        c.eval.policy.label(),
                        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                    ),
                ]),
            ])
        })
        .collect();
    for (name, b) in &d.app.config.bundles {
        items.push(ListItem::new(vec![
            Line::from(vec![
                Span::styled(
                    "▣",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    name.clone(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    b.components.join(" · "),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ),
            ]),
        ]));
    }
    d.list_area = area;

    let mut list_state = ListState::default();
    list_state.select(Some(d.selected));

    let list = List::new(items)
        .block(bordered("Components"))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(0x1e, 0x29, 0x3b))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");

    f.render_stateful_widget(list, area, &mut list_state);
}

/// One glyph that says what this component needs from you.
fn decision_marker(c: &ComponentStatus) -> (&'static str, Color) {
    match &c.eval.decision {
        Decision::Gated { .. } => ("◆", GATED),
        Decision::Apply { .. } if c.eval.gated.is_some() => ("◆", GATED),
        Decision::Apply { .. } => ("↑", AUTO),
        Decision::Held { .. } => ("■", MUTED),
        Decision::Soaking { .. } => ("◔", MUTED),
        Decision::UpToDate if c.eval.series_withdrawn => ("!", GATED),
        Decision::UpToDate if c.upstream_eol_days.is_some() => ("!", BAD),
        Decision::UpToDate => ("•", GOOD),
    }
}

fn draw_detail(f: &mut Frame, area: Rect, d: &mut Dashboard) {
    let text = match d.tab {
        Tab::Overview => overview_text(d),
        Tab::Lineage => Text::default(),
        Tab::Health => health_text(d),
        Tab::Log => log_text(d),
    };

    let title = d
        .selected_name()
        .map(|n| format!("{} — {}", n, d.tab.title()))
        .unwrap_or_else(|| d.tab.title().to_string());

    f.render_widget(
        Paragraph::new(text)
            .block(bordered(&title))
            .wrap(Wrap { trim: false })
            .scroll((d.scroll, 0)),
        area,
    );
}

fn overview_text(d: &Dashboard) -> Text<'static> {
    if let Some(bundle) = d.selected_bundle() {
        let mut t = bundle_text(d, &bundle);
        t.lines.extend(system_lines(d));
        return t;
    }
    let Some(c) = d.selected_component() else {
        return Text::from("no components configured");
    };
    let cfg = d.app.config.components.get(&c.eval.component);

    let mut lines = Vec::new();

    lines.push(kv(
        "installed",
        c.eval
            .installed
            .as_ref()
            .map_or_else(|| "—".into(), Evr::to_string),
    ));
    lines.push(kv(
        "series",
        c.eval.series.clone().unwrap_or_else(|| "—".into()),
    ));
    lines.push(kv("policy", c.eval.policy.label().to_string()));

    // Lifecycle and rollback targets only mean something where sluice gates,
    // or where a known-good version has been recorded anyway (a bundle member).
    let gates = c.eval.policy != crate::config::Policy::Follow || c.known_good.is_some();
    if gates {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", "state"), Style::default().fg(MUTED)),
            match c.lifecycle {
                Lifecycle::Unvetted => Span::styled("unvetted", Style::default().fg(MUTED)),
                Lifecycle::KnownGood => Span::styled("known-good", Style::default().fg(GOOD)),
                Lifecycle::Testing => Span::styled("testing", Style::default().fg(GATED)),
            },
        ]));
    }

    lines.push(Line::raw(""));

    let (marker, color) = decision_marker(c);
    lines.push(Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(color)),
        Span::styled(
            c.headline(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]));

    lines.push(Line::raw(""));

    // The rollback target gets its own emphasis: without it, promoting is a
    // one-way trip, and that is the single most important thing to know here.
    match &c.known_good {
        Some(kg) if c.known_good_vaulted => lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", "rollback to"), Style::default().fg(MUTED)),
            Span::styled(kg.to_string(), Style::default().fg(GOOD)),
            Span::styled("  (vaulted)", Style::default().fg(MUTED)),
        ])),
        Some(kg) => lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", "rollback to"), Style::default().fg(MUTED)),
            Span::styled(kg.to_string(), Style::default().fg(GATED)),
            Span::styled("  NOT vaulted", Style::default().fg(BAD)),
        ])),
        None if gates => lines.push(Line::styled(
            "no known-good version recorded — press m to mark the installed one",
            Style::default().fg(GATED),
        )),
        None => {}
    }
    if let Some(held) = &c.eval.held_at {
        lines.push(Line::styled(
            format!("held at {held} after a rollback — press p to release it"),
            Style::default().fg(GATED),
        ));
    }

    if let Some(u) = &c.upstream {
        lines.push(kv("upstream", u.describe()));
    }
    if let Some(days) = c.upstream_eol_days {
        lines.push(Line::styled(
            format!("end-of-life upstream for {}", crate::app::eol_age(days)),
            Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(h) = &c.health {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", "boots"), Style::default().fg(MUTED)),
            Span::styled(
                h.summary(),
                Style::default().fg(if h.unclean_ends > 0 { GATED } else { GOOD }),
            ),
        ]));
    }

    if let Some(cfg) = cfg {
        let alerts = c.alerts(cfg);
        if !alerts.is_empty() {
            lines.push(Line::raw(""));
            for (urgency, msg) in alerts {
                lines.push(Line::from(vec![
                    Span::styled("• ", Style::default().fg(urgency_color(urgency))),
                    Span::styled(msg, Style::default().fg(urgency_color(urgency))),
                ]));
            }
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("packages: {}", c.eval.family_names().join(", ")),
        Style::default().fg(MUTED),
    ));
    lines.extend(system_lines(d));

    Text::from(lines)
}

fn bundle_text(d: &Dashboard, bundle: &str) -> Text<'static> {
    let members = d
        .app
        .config
        .bundles
        .get(bundle)
        .map(|b| b.components.clone())
        .unwrap_or_default();
    let mut lines = vec![
        Line::styled(
            "promoted, marked good and rolled back as one unit",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ),
        Line::raw(""),
    ];
    for m in &members {
        let Some(c) = d.component_status(m) else {
            lines.push(Line::styled(
                format!("{m:<16} not evaluated"),
                Style::default().fg(MUTED),
            ));
            continue;
        };
        let (marker, color) = decision_marker(c);
        let state = match c.lifecycle {
            Lifecycle::Unvetted => ("unvetted", MUTED),
            Lifecycle::KnownGood => ("known-good", GOOD),
            Lifecycle::Testing => ("testing", GATED),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(color)),
            Span::styled(
                format!("{m:<16}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "{:<14}",
                c.eval
                    .installed
                    .as_ref()
                    .map_or_else(|| "—".into(), |v| v.to_string())
            )),
            Span::styled(format!("{:<12}", state.0), Style::default().fg(state.1)),
            Span::styled(c.headline(), Style::default().fg(color)),
        ]));
    }
    lines.push(Line::raw(""));
    let hint = [
        ("p", "promote together"),
        ("m", "mark good together"),
        ("b", "roll back together"),
        ("2", "timeline of the stack"),
    ];
    let mut spans = Vec::new();
    for (k, what) in hint {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::default().fg(Color::Black).bg(ACCENT),
        ));
        spans.push(Span::styled(
            format!(" {what}   "),
            Style::default().fg(MUTED),
        ));
    }
    lines.push(Line::from(spans));
    Text::from(lines)
}

/// The machine-wide facts, under every overview.
fn system_lines(d: &Dashboard) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            "system",
            Style::default()
                .fg(MUTED)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
    ];
    let Some(status) = &d.status else {
        return lines;
    };
    if let Some(esp) = &status.esp {
        let color = if status.esp_low { GATED } else { MUTED };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", "ESP"), Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{} MB free of {} MB ({}% used) at {}",
                    esp.free_mb,
                    esp.total_mb,
                    esp.used_percent(),
                    esp.path.display()
                ),
                Style::default().fg(color),
            ),
        ]));
        // A bar, because 82% reads faster as a shape than as a number.
        let width = 30usize;
        let used = (esp.used_percent() as usize * width / 100).min(width);
        lines.push(Line::from(vec![
            Span::raw(format!("{:<14}", "")),
            Span::styled("█".repeat(used), Style::default().fg(color)),
            Span::styled(
                "░".repeat(width - used),
                Style::default().fg(Color::Rgb(0x33, 0x41, 0x55)),
            ),
        ]));
    }
    if let Some(id) = crate::boot::default_entry_from_efivars(&d.app.config.boot.efivars_dir) {
        lines.push(kv("boot default", id));
    }
    if let Some(k) = &d.app.running_kernel {
        lines.push(kv("running", k.clone()));
    }
    if status.vault_bytes > 0 {
        lines.push(kv(
            "vault",
            format!("{:.1} MB", status.vault_bytes as f64 / 1_048_576.0),
        ));
    }
    for w in &status.warnings {
        lines.push(Line::styled(
            format!("note: {w}"),
            Style::default().fg(MUTED),
        ));
    }
    lines
}

fn health_text(d: &Dashboard) -> Text<'static> {
    let Some(h) = &d.health else {
        return Text::styled("no boot data", Style::default().fg(MUTED));
    };
    if let Some(reason) = &h.unavailable {
        return Text::styled(reason.clone(), Style::default().fg(GATED));
    }

    let mut lines = vec![
        Line::styled(
            "evidence per kernel — sluice never marks a kernel good on its own",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ),
        Line::raw(""),
    ];
    if let Some(note) = &h.kernels_unknown {
        lines.push(Line::styled(note.clone(), Style::default().fg(GATED)));
        lines.push(Line::raw(""));
    }

    for k in h.by_kernel() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<22}", k.kernel),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                k.summary(),
                Style::default().fg(if k.unclean_ends > 0 { GATED } else { GOOD }),
            ),
        ]));
    }

    let unclean = h.unclean();
    if !unclean.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "boots that ended without a shutdown sequence",
            Style::default().fg(GATED).add_modifier(Modifier::BOLD),
        ));
        for b in unclean {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(crate::health::local_window(b), Style::default().fg(MUTED)),
                Span::raw("  "),
                Span::raw(b.kernel.clone().unwrap_or_else(|| "unknown".into())),
                if b.pstore_hits > 0 {
                    Span::styled(
                        format!("  {} pstore record(s)", b.pstore_hits),
                        Style::default().fg(BAD),
                    )
                } else {
                    Span::raw("")
                },
            ]));
        }
    }

    Text::from(lines)
}

fn log_text(d: &Dashboard) -> Text<'static> {
    let mut lines = vec![Line::styled(
        "this session",
        Style::default()
            .fg(MUTED)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )];
    if d.log.is_empty() {
        lines.push(Line::styled("nothing yet", Style::default().fg(MUTED)));
    }
    lines.extend(
        d.log
            .iter()
            .rev()
            .map(|l| Line::styled(l.clone(), Style::default().fg(MUTED))),
    );

    // The durable record: every command sluice has run to change the system.
    let path = &d.app.config.paths.log_file;
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("action log — {}", path.display()),
        Style::default()
            .fg(MUTED)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ));
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let tail: Vec<&str> = text.lines().rev().take(60).collect();
            if tail.is_empty() {
                lines.push(Line::styled("empty", Style::default().fg(MUTED)));
            }
            for l in tail {
                let color = if l.contains(" DRY-RUN ") {
                    MUTED
                } else if l.contains(" RUN ") {
                    AUTO
                } else {
                    MUTED
                };
                lines.push(Line::styled(l.to_string(), Style::default().fg(color)));
            }
        }
        Err(e) => lines.push(Line::styled(
            format!("not readable: {e}"),
            Style::default().fg(MUTED),
        )),
    }
    Text::from(lines)
}

fn draw_footer(f: &mut Frame, area: Rect, d: &Dashboard) {
    let keys: &[(&str, &str)] = match d.modal {
        Modal::None if d.tab == Tab::Lineage => &[
            ("j/k", "component"),
            ("tab", "pane"),
            ("r", "refresh"),
            ("u", "update"),
            ("p", "promote"),
            ("m", "mark good"),
            ("b", "roll back"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Modal::None => &[
            ("↑↓/jk", "select"),
            ("tab", "pane"),
            ("l", "timeline"),
            ("r", "refresh"),
            ("u", "update"),
            ("p", "promote"),
            ("m", "mark good"),
            ("b", "roll back"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Modal::Confirm(_) => &[("y", "confirm"), ("n/esc", "cancel")],
        _ => &[("esc", "close")],
    };

    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(MUTED),
        ));
        spans.push(Span::styled(
            format!(" {label}  "),
            Style::default().fg(MUTED),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_confirm(f: &mut Frame, action: &super::state::PendingAction) {
    let area = centered(70, 40, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::styled(
            action.consequence.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            "this will run, with privileges:",
            Style::default().fg(MUTED),
        ),
        Line::styled(action.command_line.clone(), Style::default().fg(AUTO)),
        Line::raw(""),
        Line::styled(
            "the terminal is handed over so the password prompt works normally",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ),
    ];

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(GATED))
                .title(Span::styled(
                    format!(" {} ", action.title),
                    Style::default().fg(GATED).add_modifier(Modifier::BOLD),
                )),
        ),
        area,
    );
}

fn draw_output(f: &mut Frame, title: &str, body: &str, ok: bool) {
    let area = centered(80, 70, f.area());
    f.render_widget(Clear, area);

    // The tail is what matters after a long zypper transaction.
    let tail: Vec<&str> = body.lines().rev().take(200).collect();
    let lines: Vec<Line> = tail
        .into_iter()
        .rev()
        .map(|l| Line::raw(l.to_string()))
        .collect();

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if ok { GOOD } else { BAD }))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default()
                        .fg(if ok { GOOD } else { BAD })
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let area = centered(64, 90, f.area());
    f.render_widget(Clear, area);

    let rows = [
        ("↑ ↓ / j k", "select a component"),
        ("tab / shift-tab", "switch pane"),
        ("1 2 3 4", "jump to a pane"),
        ("l", "open the timeline"),
        ("r", "re-read the system"),
        ("mouse", "click to select, wheel to zoom, drag to pan"),
        ("", ""),
        ("timeline", ""),
        ("  ← → / h l", "scrub release by release"),
        ("  ↑ ↓", "move between series"),
        ("  ⇧← ⇧→ / H L", "pan"),
        ("  + −", "zoom in and out"),
        ("  [ ]", "less / more information"),
        ("  enter", "all the detail, and back"),
        ("  c g t f", "current · gated · today · fit everything"),
        ("", ""),
        ("u", "update — apply what policy allows"),
        ("p", "promote — cross the series gate (explicit)"),
        ("m", "mark good — vault and pin the installed version"),
        ("b", "roll back — boot the known-good version again"),
        ("", ""),
        ("pgup/pgdn", "scroll the detail pane"),
        ("q / ctrl-c", "quit"),
    ];

    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(
                    format!("  {key:<17}"),
                    Style::default().fg(AUTO).add_modifier(Modifier::BOLD),
                ),
                Span::raw((*what).to_string()),
            ])
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(AUTO))
                .title(Span::styled(
                    " keys ",
                    Style::default().fg(AUTO).add_modifier(Modifier::BOLD),
                ))
                .title_bottom(
                    Line::from(Span::styled(" esc to close ", Style::default().fg(MUTED)))
                        .alignment(Alignment::Center),
                ),
        ),
        area,
    );
}

fn bordered(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

fn kv(key: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), Style::default().fg(MUTED)),
        Span::raw(value),
    ])
}

fn urgency_color(u: crate::notify::Urgency) -> Color {
    match u {
        crate::notify::Urgency::Info => AUTO,
        crate::notify::Urgency::Warning => GATED,
        crate::notify::Urgency::Critical => BAD,
    }
}

/// A centred rectangle, `pct_x` by `pct_y` percent of `area`.
fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_is_inside_its_parent() {
        let parent = Rect::new(0, 0, 100, 40);
        let r = centered(70, 40, parent);
        assert!(r.width <= parent.width && r.height <= parent.height);
        assert!(r.x >= parent.x && r.y >= parent.y);
        assert!(r.right() <= parent.right());
        assert!(r.bottom() <= parent.bottom());
    }

    #[test]
    fn centered_rect_survives_a_tiny_terminal() {
        let r = centered(70, 40, Rect::new(0, 0, 4, 2));
        assert!(r.width <= 4 && r.height <= 2);
    }
}
