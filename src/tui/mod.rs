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
pub mod ui;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    execute!(stdout, EnterAlternateScreen).context("entering the alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("initialising the terminal")
}

fn leave(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(terminal: &mut Tui, d: &mut Dashboard) -> Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, d))?;

        // A poll rather than a blocking read, so a future background refresh
        // can redraw without an input event to wake it.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                handle_key(terminal, d, key)?;
            }
            Event::Resize(_, _) => {}
            _ => {}
        }

        if d.should_quit {
            return Ok(());
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
                KeyCode::Char('y') | KeyCode::Char('Y') => {
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

    match key.code {
        KeyCode::Char('q') => d.should_quit = true,
        KeyCode::Char('?') => d.modal = Modal::Help,

        KeyCode::Down | KeyCode::Char('j') => d.select_next(),
        KeyCode::Up | KeyCode::Char('k') => d.select_prev(),

        KeyCode::Tab => {
            d.tab = d.tab.next();
            d.scroll = 0;
        }
        KeyCode::BackTab => {
            d.tab = d.tab.prev();
            d.scroll = 0;
        }
        KeyCode::Char(c @ '1'..='4') => {
            d.tab = Tab::ALL[(c as u8 - b'1') as usize];
            d.scroll = 0;
        }

        KeyCode::PageDown => d.scroll = d.scroll.saturating_add(10),
        KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(10),
        KeyCode::Home => d.scroll = 0,

        KeyCode::Char('l') => {
            d.tab = Tab::Lineage;
            // Drawn once first, so the "fetching…" note is on screen while the
            // network call blocks rather than appearing after it returns.
            terminal.draw(|f| ui::draw(f, d))?;
            d.ensure_lineage();
        }

        KeyCode::Char('r') => d.refresh(),

        KeyCode::Char('u') => propose(d, Verb::Update),
        KeyCode::Char('p') => propose(d, Verb::Promote),
        KeyCode::Char('m') => propose(d, Verb::MarkGood),
        KeyCode::Char('b') => propose(d, Verb::Rollback),

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
