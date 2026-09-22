//! Command execution with dry-run support and an audit trail.
//!
//! Every mutating command in sluice goes through a [`Runner`], so that
//! `--dry-run` is enforced in one place rather than at each call site, and so
//! the exact argv of anything that touched the system lands in the action log.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn require_ok(&self, what: &str) -> Result<&Self> {
        anyhow::ensure!(
            self.ok(),
            "{what} failed with exit status {}:\n{}",
            self.status,
            self.stderr.trim()
        );
        Ok(self)
    }
}

/// A command about to be run, kept as data so it can be logged or printed
/// verbatim under `--dry-run`.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Mutating commands are suppressed under `--dry-run`; read-only ones are not.
    pub mutating: bool,
}

impl Cmd {
    pub fn read(program: impl Into<OsString>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            mutating: false,
        }
    }

    pub fn mutate(program: impl Into<OsString>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            mutating: true,
        }
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(items.into_iter().map(|s| s.as_ref().to_os_string()));
        self
    }

    /// Shell-ish rendering for logs and dry-run output. Quoting is good enough
    /// to be unambiguous to a reader; it is never fed back to a shell.
    /// Program and arguments as one list, for passing a command to a wrapper
    /// such as `setpriv`.
    pub fn argv(&self) -> Vec<OsString> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }

    pub fn display(&self) -> String {
        let mut out = self.program.to_string_lossy().into_owned();
        for a in &self.args {
            let a = a.to_string_lossy();
            out.push(' ');
            if a.is_empty() || a.contains([' ', '\t', '\'', '"', '$', '&', '|', ';', '<', '>']) {
                out.push_str(&format!("'{}'", a.replace('\'', r"'\''")));
            } else {
                out.push_str(&a);
            }
        }
        out
    }
}

pub struct Runner {
    dry_run: bool,
    log_file: Option<PathBuf>,
    /// Commands recorded this session, in order. Used by tests and by the TUI's
    /// transcript pane.
    transcript: Vec<String>,
    /// An unwritable log is reported once, not once per command.
    log_warned: bool,
}

impl Runner {
    pub fn new(dry_run: bool, log_file: Option<PathBuf>) -> Self {
        Runner {
            dry_run,
            log_file,
            transcript: Vec::new(),
            log_warned: false,
        }
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn transcript(&self) -> &[String] {
        &self.transcript
    }

    /// Run `cmd`, capturing its output.
    ///
    /// Under `--dry-run` a mutating command is not executed; it is logged and a
    /// synthetic success is returned. Read-only commands always run, since
    /// `--dry-run` is about not changing the system, not about not looking at it.
    pub fn run(&mut self, cmd: &Cmd) -> Result<Output> {
        let rendered = cmd.display();
        if cmd.mutating {
            self.audit(&rendered)?;
        }
        self.transcript.push(rendered.clone());

        if self.dry_run && cmd.mutating {
            return Ok(Output {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        let out = Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running `{rendered}`"))?;

        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Append a line to the action log. A log that cannot be written is not
    /// fatal — losing the audit trail must not block a rollback — but it is
    /// reported to stderr so the problem is visible.
    fn audit(&mut self, line: &str) -> Result<()> {
        let Some(path) = &self.log_file else {
            return Ok(());
        };
        let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z");
        let prefix = if self.dry_run { "DRY-RUN" } else { "RUN" };
        if let Err(e) = append_line(path, &format!("{stamp} {prefix} {line}")) {
            if !self.log_warned {
                eprintln!(
                    "warning: could not write action log {}: {e:#}",
                    path.display()
                );
            }
            self.log_warned = true;
        }
        Ok(())
    }

    /// Record a non-command event (a state transition, a decision) in the log.
    pub fn note(&mut self, msg: &str) {
        let Some(path) = &self.log_file else { return };
        let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z");
        let _ = append_line(path, &format!("{stamp} NOTE {msg}"));
    }
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_suppresses_mutating_commands_only() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("touched");
        let mut r = Runner::new(true, None);

        let mutating = Cmd::mutate("touch").arg(&marker);
        r.run(&mutating).unwrap();
        assert!(!marker.exists(), "dry-run must not run mutating commands");

        let reading = Cmd::read("true");
        assert!(r.run(&reading).unwrap().ok());
    }

    #[test]
    fn display_quotes_arguments_with_spaces() {
        let cmd = Cmd::read("zypper")
            .arg("addlock")
            .arg("kernel-default >= 7.3");
        assert_eq!(cmd.display(), "zypper addlock 'kernel-default >= 7.3'");
    }

    #[test]
    fn mutating_commands_are_appended_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sluice.log");
        let mut r = Runner::new(true, Some(log.clone()));
        // Mutating commands are not executed under dry run; the read-only one
        // is, so it must exist everywhere the tests run.
        r.run(&Cmd::mutate("zypper").arg("dup")).unwrap();
        r.run(&Cmd::read("true").arg("--read-only-marker")).unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("DRY-RUN zypper dup"));
        assert!(
            !text.contains("--read-only-marker"),
            "read-only commands are not audited"
        );
    }
}
