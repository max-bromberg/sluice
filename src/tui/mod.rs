//! The interactive dashboard.
//!
//! The TUI runs unprivileged. Reading the system needs no privileges, and a
//! full-screen program running as root is a large surface for something whose
//! job is mostly to display text. When you ask for a change, sluice hands the
//! terminal back, re-executes *itself* under `pkexec` or `sudo` for that one
//! subcommand — showing you the exact argv first — and then takes the screen
//! back and re-reads the system. There is no long-lived privileged session, and
//! nothing is escalated that you have not seen written out.

pub mod state;
pub mod timeline;
pub mod ui;
pub mod worker;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::Config;
use state::{Dashboard, Modal, Tab, Verb};

type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn run(config: Config, dry_run: bool) -> Result<i32> {
    let mut dashboard = Dashboard::new(config, dry_run)?;

    // Anything an interrupted previous run left unlocked is put back before the
    // dashboard claims the system is in a known state.
    match dashboard.app.repair_locks() {
        Ok(Some(msg)) => dashboard.note(msg),
        Ok(None) => {}
        Err(e) => dashboard.note(format!("lock repair failed: {e:#}")),
    }
    dashboard.refresh();

    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, &mut dashboard);
    leave(&mut terminal)?;

    // Errors are reported after the terminal is restored, or the message lands
    // on a screen that is about to be torn down.
    result?;
    Ok(0)
}

fn enter() -> Result<Tui> {
    enable_raw_mode().context("entering raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("entering the alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("initialising the terminal")
}

fn leave(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(terminal: &mut Tui, d: &mut Dashboard) -> Result<()> {
    loop {
        d.pump();
        let animating = if d.tab == Tab::Lineage {
            d.ensure_timeline();
            match d.active_timeline() {
                Some(tl) => {
                    tl.step();
                    tl.animating()
                }
                None => false,
            }
        } else {
            false
        };

        terminal.draw(|f| ui::draw(f, d))?;
        // With the placeholder on screen, do the slow local reads.
        if d.tab == Tab::Lineage {
            d.read_machine();
        }

        // ~60 fps while something moves; otherwise slow enough to be idle but
        // quick enough for the running marker to keep pulsing.
        let wait = Duration::from_millis(if animating { 16 } else { 120 });
        if !event::poll(wait)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                handle_key(terminal, d, key)?;
            }
            Event::Mouse(m) => handle_mouse(d, m),
            Event::Resize(_, _) => {}
            _ => {}
        }

        if d.should_quit {
            return Ok(());
        }
    }
}

fn handle_mouse(d: &mut Dashboard, m: MouseEvent) {
    if !matches!(d.modal, Modal::None) {
        return;
    }
    let inside = |r: ratatui::layout::Rect| {
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
    };
    if let MouseEventKind::Down(MouseButton::Left) = m.kind {
        // The tab bar: each title is its name plus a divider.
        if inside(d.tabs_area) {
            let mut x = d.tabs_area.x;
            for t in Tab::ALL {
                let w = t.title().chars().count() as u16 + 3;
                if m.column < x + w {
                    d.tab = t;
                    d.scroll = 0;
                    return;
                }
                x += w;
            }
            return;
        }
        // The component list: two lines per entry, inside a border.
        if inside(d.list_area) && m.row > d.list_area.y {
            let i = usize::from((m.row - d.list_area.y - 1) / 2);
            if i < d.entries().len() {
                d.selected = i;
                d.scroll = 0;
            }
            return;
        }
    }
    if d.tab == Tab::Lineage {
        if let Some(tl) = d.active_timeline() {
            tl.handle_mouse(m);
            let info = tl.info;
            d.info_level = info;
        }
    } else {
        match m.kind {
            MouseEventKind::ScrollDown => d.scroll = d.scroll.saturating_add(3),
            MouseEventKind::ScrollUp => d.scroll = d.scroll.saturating_sub(3),
            _ => {}
        }
    }
}

fn handle_key(terminal: &mut Tui, d: &mut Dashboard, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        d.should_quit = true;
        return Ok(());
    }

    // A modal owns the keyboard while it is open.
    match d.modal.clone() {
        Modal::Confirm(action) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if !action.blocked => {
                    d.modal = Modal::None;
                    run_privileged(terminal, d, &action)?;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    d.modal = Modal::None;
                    d.note(format!("cancelled: {}", action.title));
                }
                _ => {}
            }
            return Ok(());
        }
        Modal::Help | Modal::Output { .. } => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                d.modal = Modal::None;
            }
            return Ok(());
        }
        Modal::None => {}
    }

    // Keys that mean the same everywhere.
    match key.code {
        KeyCode::Char('q') => {
            d.should_quit = true;
            return Ok(());
        }
        KeyCode::Char('?') => {
            d.modal = Modal::Help;
            return Ok(());
        }
        KeyCode::Char('j') => {
            d.select_next();
            return Ok(());
        }
        KeyCode::Char('k') => {
            d.select_prev();
            return Ok(());
        }
        KeyCode::Tab => {
            d.tab = d.tab.next();
            d.scroll = 0;
            return Ok(());
        }
        KeyCode::BackTab => {
            d.tab = d.tab.prev();
            d.scroll = 0;
            return Ok(());
        }
        KeyCode::Char(c @ '1'..='4') => {
            d.tab = Tab::ALL[(c as u8 - b'1') as usize];
            d.scroll = 0;
            return Ok(());
        }
        KeyCode::Char('r') => {
            d.refresh();
            return Ok(());
        }
        KeyCode::Char('u') => {
            propose(d, Verb::Update);
            return Ok(());
        }
        KeyCode::Char('p') => {
            propose(d, Verb::Promote);
            return Ok(());
        }
        KeyCode::Char('m') => {
            propose(d, Verb::MarkGood);
            return Ok(());
        }
        KeyCode::Char('b') => {
            propose(d, Verb::Rollback);
            return Ok(());
        }
        KeyCode::Char('U') => {
            propose(d, Verb::SelfUpdate);
            return Ok(());
        }
        _ => {}
    }

    if d.tab == Tab::Lineage {
        if let Some(tl) = d.active_timeline() {
            tl.handle_key(key);
            let info = tl.info;
            d.info_level = info;
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Down => d.select_next(),
        KeyCode::Up => d.select_prev(),
        KeyCode::PageDown => d.scroll = d.scroll.saturating_add(10),
        KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(10),
        KeyCode::Home => d.scroll = 0,
        KeyCode::Char('l') | KeyCode::Enter => d.tab = Tab::Lineage,
        _ => {}
    }
    Ok(())
}

fn propose(d: &mut Dashboard, verb: Verb) {
    match d.plan(verb) {
        Some(action) => d.modal = Modal::Confirm(action),
        None => d.note(format!(
            "nothing to {} for the selected component",
            verb.label()
        )),
    }
}

/// Hand the terminal to an escalated `sluice` invocation, then take it back.
///
/// The child inherits stdio, so `pkexec`'s agent or `sudo`'s password prompt
/// behaves exactly as it would at a shell. The alternate screen is left first
/// and restored after, so the prompt is not drawn over the dashboard.
fn run_privileged(
    terminal: &mut Tui,
    d: &mut Dashboard,
    action: &super::tui::state::PendingAction,
) -> Result<()> {
    use std::process::{Command, Stdio};

    leave(terminal)?;

    println!("\n$ {}\n", action.command_line);

    let exe = crate::privilege::current_exe()?;
    let config = d.app.config.source.clone();
    let cmd = crate::privilege::escalated(d.escalator, &exe, config.as_deref(), &action.args);

    let status = Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    let (ok, summary) = match status {
        Ok(s) if s.success() => (true, format!("{} finished", action.title)),
        Ok(s) => (
            false,
            format!(
                "{} exited with status {}",
                action.title,
                s.code().unwrap_or(-1)
            ),
        ),
        Err(e) => (false, format!("{} could not be started: {e}", action.title)),
    };

    println!("\n{summary}\npress Enter to return to sluice…");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);

    *terminal = enter()?;
    terminal.clear()?;

    d.note(summary.clone());
    d.modal = Modal::Output {
        title: action.title.clone(),
        body: summary,
        ok,
    };

    // The escalated process changed state on disk; re-read all of it rather
    // than guessing what moved.
    d.app.state = crate::state::State::load(&d.app.config.paths.state_file())?;
    d.refresh();
    Ok(())
}
