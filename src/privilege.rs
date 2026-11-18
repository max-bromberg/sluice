//! The privilege boundary.
//!
//! Reading is unprivileged; changing the system is not. Rather than running the
//! whole TUI as root, sluice runs as the user and re-executes *itself* for a
//! single mutating subcommand. The escalated process gets an explicit argv and
//! nothing else — no inherited terminal state, no long-lived root session.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::exec::Cmd;

pub fn is_root() -> bool {
    // Avoiding a libc dependency: the effective uid is in /proc/self/status.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .is_some_and(|euid| euid == "0")
}

pub fn require_root(command: &str) -> Result<()> {
    anyhow::ensure!(
        is_root(),
        "`sluice {command}` changes the system and must run as root. Try `{}`.",
        sudo(command)
    );
    Ok(())
}

/// `sudo sluice <args>`, spelled so that sudo finds it.
///
/// sudo looks commands up in its `secure_path`, not the user's PATH, and
/// Tumbleweed's default leaves out /usr/local/bin, where sluice installs
/// itself: there, a bare `sudo sluice` is "command not found". Outside the
/// directories every `secure_path` has, the full path is given.
pub fn sudo(args: &str) -> String {
    format!(
        "sudo {} {args}",
        sudo_program(current_exe().ok().as_deref())
    )
}

fn sudo_program(exe: Option<&std::path::Path>) -> String {
    const ALWAYS_ON_SECURE_PATH: [&str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];
    match exe {
        Some(p)
            if !p.parent().is_some_and(|d| {
                ALWAYS_ON_SECURE_PATH
                    .iter()
                    .any(|s| d == std::path::Path::new(s))
            }) =>
        {
            p.display().to_string()
        }
        _ => "sluice".to_string(),
    }
}

/// Which escalation helper to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalator {
    Pkexec,
    Sudo,
    /// Already root, or no helper available.
    None,
}

impl Escalator {
    pub fn program(self) -> Option<&'static str> {
        match self {
            Escalator::Pkexec => Some("pkexec"),
            Escalator::Sudo => Some("sudo"),
            Escalator::None => None,
        }
    }
}

/// Pick an escalation helper.
///
/// `pkexec` is preferred because it prompts through the desktop's polkit agent
/// instead of taking over the terminal, which matters when the caller is a
/// full-screen TUI.
pub fn detect(preference: &str) -> Escalator {
    if is_root() {
        return Escalator::None;
    }
    match preference {
        "pkexec" => Escalator::Pkexec,
        "sudo" => Escalator::Sudo,
        "none" => Escalator::None,
        _ => {
            if which("pkexec").is_some() {
                Escalator::Pkexec
            } else if which("sudo").is_some() {
                Escalator::Sudo
            } else {
                Escalator::None
            }
        }
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| PathBuf::from(d).join(program))
        .find(|p| p.is_file())
}

/// Build the command that re-runs sluice with privileges.
///
/// The config path is passed explicitly, because the escalated process will not
/// have the invoking user's `$XDG_CONFIG_HOME` and would otherwise silently
/// fall back to a different configuration than the one on screen.
pub fn escalated(
    escalator: Escalator,
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    args: &[String],
) -> Cmd {
    let mut argv: Vec<OsString> = Vec::new();
    if let Some(program) = escalator.program() {
        argv.push(program.into());
    }
    argv.push(exe.as_os_str().to_os_string());
    if let Some(c) = config {
        argv.push("--config".into());
        argv.push(c.as_os_str().to_os_string());
    }
    argv.extend(args.iter().map(OsString::from));

    let mut cmd = Cmd::mutate(argv.remove(0));
    cmd.args = argv;
    cmd
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("locating the running sluice binary")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn explicit_preference_wins_over_detection() {
        if is_root() {
            return; // Detection short-circuits as root; nothing to assert.
        }
        assert_eq!(detect("sudo"), Escalator::Sudo);
        assert_eq!(detect("pkexec"), Escalator::Pkexec);
        assert_eq!(detect("none"), Escalator::None);
    }

    #[test]
    fn escalated_command_carries_the_config_path() {
        let cmd = escalated(
            Escalator::Sudo,
            Path::new("/usr/bin/sluice"),
            Some(Path::new("/etc/sluice/config.toml")),
            &["promote".into(), "kernel".into()],
        );
        assert_eq!(
            cmd.display(),
            "sudo /usr/bin/sluice --config /etc/sluice/config.toml promote kernel"
        );
        assert!(cmd.mutating);
    }

    #[test]
    fn already_root_invokes_the_binary_directly() {
        let cmd = escalated(
            Escalator::None,
            Path::new("/usr/bin/sluice"),
            None,
            &["update".into()],
        );
        assert_eq!(cmd.display(), "/usr/bin/sluice update");
    }

    #[test]
    fn sudo_is_given_a_path_it_can_find() {
        use std::path::Path;
        assert_eq!(sudo_program(Some(Path::new("/usr/bin/sluice"))), "sluice");
        assert_eq!(
            sudo_program(Some(Path::new("/usr/local/bin/sluice"))),
            "/usr/local/bin/sluice"
        );
        assert_eq!(sudo_program(None), "sluice");
    }
}
