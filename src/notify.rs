//! Notifications.
//!
//! The webhook URL is never stored in configuration — only the path to a
//! root-readable file containing it. That keeps a published config file, and a
//! published repository, free of a secret that would otherwise be trivial to
//! leak.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{NotifyConfig, WebhookFormat};
use crate::exec::{Cmd, Runner};

/// How much the user needs to care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Info,
    Warning,
    Critical,
}

impl Urgency {
    fn notify_send(self) -> &'static str {
        match self {
            Urgency::Info => "normal",
            Urgency::Warning => "normal",
            Urgency::Critical => "critical",
        }
    }

    /// Discord embed colour.
    fn color(self) -> u32 {
        match self {
            Urgency::Info => 0x3b_82_f6,
            Urgency::Warning => 0xf5_9e_0b,
            Urgency::Critical => 0xef_44_44,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
}

impl Notification {
    pub fn new(title: impl Into<String>, body: impl Into<String>, urgency: Urgency) -> Self {
        Notification {
            title: title.into(),
            body: body.into(),
            urgency,
        }
    }

    pub fn plain(&self) -> String {
        format!("{}\n{}", self.title, self.body)
    }
}

/// Deliver a notification over every configured channel.
///
/// Failures are collected and returned rather than propagated: a webhook that
/// is down must not make `check` fail, or a timer-driven run would start
/// reporting failures for a reason that has nothing to do with the system's
/// update state.
pub fn send(cfg: &NotifyConfig, r: &mut Runner, n: &Notification) -> Vec<String> {
    let mut problems = Vec::new();

    if let Some(path) = &cfg.webhook_file {
        match webhook_url(path) {
            Ok(url) => {
                if r.dry_run() {
                    r.note(&format!("would POST to webhook: {}", n.title));
                } else if let Err(e) = post_webhook(&url, cfg.webhook_format, n) {
                    problems.push(format!("webhook: {e:#}"));
                }
            }
            Err(e) => problems.push(format!("webhook file: {e:#}")),
        }
    }

    if cfg.desktop {
        if let Err(e) = desktop(r, n) {
            problems.push(format!("desktop notification: {e:#}"));
        }
    }

    problems
}

/// Read a webhook URL from a file, taking the first non-empty, non-comment line.
pub fn webhook_url(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading webhook file {}", path.display()))?;
    let url = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .with_context(|| format!("webhook file {} contains no URL", path.display()))?;
    anyhow::ensure!(
        url.starts_with("https://"),
        "webhook URL in {} is not https",
        path.display()
    );
    Ok(url.to_string())
}

#[derive(Serialize)]
struct DiscordEmbed<'a> {
    title: &'a str,
    description: &'a str,
    color: u32,
}

#[derive(Serialize)]
struct DiscordPayload<'a> {
    embeds: Vec<DiscordEmbed<'a>>,
}

#[derive(Serialize)]
struct SlackPayload<'a> {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<&'a str>,
}

fn post_webhook(url: &str, format: WebhookFormat, n: &Notification) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("sluice/", env!("CARGO_PKG_VERSION")))
        .build();

    let response = match format {
        WebhookFormat::Discord => agent.post(url).send_json(DiscordPayload {
            embeds: vec![DiscordEmbed {
                title: &n.title,
                description: &n.body,
                color: n.urgency.color(),
            }],
        }),
        WebhookFormat::Slack => agent.post(url).send_json(SlackPayload {
            text: format!("*{}*\n{}", n.title, n.body),
            username: Some("sluice"),
        }),
        WebhookFormat::Raw => agent.post(url).send_json(serde_json::json!({
            "title": n.title,
            "body": n.body,
            "urgency": format!("{:?}", n.urgency).to_lowercase(),
        })),
    };

    response.context("posting to webhook")?;
    Ok(())
}

fn notify_send(n: &Notification) -> Cmd {
    Cmd::read("notify-send")
        .arg("--app-name=sluice")
        .arg(format!("--urgency={}", n.urgency.notify_send()))
        .arg(&n.title)
        .arg(&n.body)
}

fn desktop(r: &mut Runner, n: &Notification) -> Result<()> {
    if crate::privilege::is_root() {
        return desktop_as_root(r, n);
    }
    // No graphical session means no desktop notification. Not an error.
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(());
    }
    let out = r.run(&notify_send(n))?;
    anyhow::ensure!(out.ok(), "notify-send failed: {}", out.stderr.trim());
    Ok(())
}

/// A graphical login, as logind reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopSession {
    uid: u32,
    name: String,
}

/// From a root timer there is no desktop to talk to, so find the people who
/// are logged in graphically and notify each on their own session bus, as
/// themselves. `setpriv` drops to the user without a PAM session; nothing runs
/// as root inside their desktop.
fn desktop_as_root(r: &mut Runner, n: &Notification) -> Result<()> {
    let list = r.run(
        &Cmd::read("loginctl")
            .arg("list-sessions")
            .arg("--no-legend"),
    )?;
    anyhow::ensure!(
        list.ok(),
        "loginctl list-sessions failed: {}",
        list.stderr.trim()
    );

    let mut sessions: Vec<DesktopSession> = Vec::new();
    for id in list
        .stdout
        .lines()
        .filter_map(|l| l.split_whitespace().next())
    {
        let props = r.run(&Cmd::read("loginctl").arg("show-session").arg(id).args([
            "-p", "User", "-p", "Name", "-p", "Type", "-p", "Class", "-p", "Active", "-p", "Remote",
        ]))?;
        if let Some(s) = parse_desktop_session(&props.stdout) {
            if !sessions.contains(&s) {
                sessions.push(s);
            }
        }
    }

    let mut failures = Vec::new();
    for s in &sessions {
        let bus = format!("/run/user/{}/bus", s.uid);
        if !std::path::Path::new(&bus).exists() {
            continue;
        }
        let send = notify_send(n);
        let cmd = Cmd::read("setpriv")
            .arg(format!("--reuid={}", s.uid))
            .arg(format!("--regid={}", primary_gid(r, s.uid)?))
            .arg("--init-groups")
            .arg("--")
            .arg("env")
            .arg(format!("DBUS_SESSION_BUS_ADDRESS=unix:path={bus}"))
            .args(send.argv());
        let out = r.run(&cmd)?;
        if !out.ok() {
            failures.push(format!("{}: {}", s.name, out.stderr.trim()));
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "notify-send failed for {}",
        failures.join("; ")
    );
    Ok(())
}

fn parse_desktop_session(props: &str) -> Option<DesktopSession> {
    let get = |key: &str| {
        props
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let graphical = matches!(get("Type"), Some("x11" | "wayland" | "mir"));
    let usable = get("Class") == Some("user")
        && get("Active") == Some("yes")
        && get("Remote") != Some("yes");
    if !(graphical && usable) {
        return None;
    }
    Some(DesktopSession {
        uid: get("User")?.parse().ok()?,
        name: get("Name")?.to_string(),
    })
}

fn primary_gid(r: &mut Runner, uid: u32) -> Result<u32> {
    let out = r.run(&Cmd::read("getent").arg("passwd").arg(uid.to_string()))?;
    out.stdout
        .split(':')
        .nth(3)
        .and_then(|g| g.trim().parse().ok())
        .with_context(|| format!("no passwd entry for uid {uid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `loginctl show-session` output from a Plasma Wayland login.
    #[test]
    fn recognises_a_local_graphical_session() {
        let props = "User=1000\nName=max\nRemote=no\nType=wayland\nClass=user\nActive=yes\n";
        assert_eq!(
            parse_desktop_session(props),
            Some(DesktopSession {
                uid: 1000,
                name: "max".into()
            })
        );
    }

    #[test]
    fn ignores_managers_ttys_and_remote_logins() {
        let manager =
            "User=1000\nName=max\nRemote=no\nType=unspecified\nClass=manager\nActive=yes\n";
        let tty = "User=1000\nName=max\nRemote=no\nType=tty\nClass=user\nActive=yes\n";
        let ssh = "User=1000\nName=max\nRemote=yes\nType=x11\nClass=user\nActive=yes\n";
        let background = "User=1000\nName=max\nRemote=no\nType=wayland\nClass=user\nActive=no\n";
        for p in [manager, tty, ssh, background] {
            assert_eq!(parse_desktop_session(p), None, "{p}");
        }
    }

    /// The spec's acceptance test: a gated series reaches Discord. The real
    /// HTTP path is exercised against a local listener, and the payload must be
    /// the embed shape Discord's webhook API accepts.
    #[test]
    fn a_gated_series_is_posted_to_a_discord_webhook() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/api/webhooks/1/abc",
            listener.local_addr().unwrap()
        );

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            (request_line, String::from_utf8(body).unwrap())
        });

        let n = Notification::new(
            "sluice: updates need your attention",
            "• kernel: a new 7.3 series is available (7.3.3-1.1), held for your decision",
            Urgency::Info,
        );
        post_webhook(&url, WebhookFormat::Discord, &n).unwrap();

        let (request_line, body) = server.join().unwrap();
        assert!(
            request_line.starts_with("POST /api/webhooks/1/abc"),
            "{request_line}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let embed = &json["embeds"][0];
        assert_eq!(embed["title"], "sluice: updates need your attention");
        assert!(embed["description"]
            .as_str()
            .unwrap()
            .contains("7.3 series"));
        assert!(embed["color"].is_u64());
    }

    #[test]
    fn reads_the_first_real_line_of_a_webhook_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webhook");
        std::fs::write(
            &path,
            "# the Discord webhook for this host\n\nhttps://discord.com/api/webhooks/1/abc\n",
        )
        .unwrap();
        assert_eq!(
            webhook_url(&path).unwrap(),
            "https://discord.com/api/webhooks/1/abc"
        );
    }

    #[test]
    fn rejects_a_plaintext_webhook_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webhook");
        std::fs::write(&path, "http://example.com/hook\n").unwrap();
        assert!(webhook_url(&path)
            .unwrap_err()
            .to_string()
            .contains("not https"));
    }

    #[test]
    fn an_empty_webhook_file_is_an_error_not_an_empty_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webhook");
        std::fs::write(&path, "# only a comment\n").unwrap();
        assert!(webhook_url(&path)
            .unwrap_err()
            .to_string()
            .contains("no URL"));
    }

    #[test]
    fn a_missing_webhook_file_is_reported_but_never_panics() {
        let cfg = NotifyConfig {
            webhook_file: Some("/nonexistent/sluice/webhook".into()),
            desktop: false,
            ..Default::default()
        };
        let mut r = Runner::new(true, None);
        let problems = send(&cfg, &mut r, &Notification::new("t", "b", Urgency::Info));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("webhook file:"));
    }

    #[test]
    fn no_channels_configured_means_no_problems() {
        let cfg = NotifyConfig {
            webhook_file: None,
            desktop: false,
            ..Default::default()
        };
        let mut r = Runner::new(true, None);
        assert!(send(&cfg, &mut r, &Notification::new("t", "b", Urgency::Info)).is_empty());
    }
}
