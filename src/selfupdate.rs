//! Updating sluice itself — on the same terms it holds everything else to.
//!
//! A tool whose job is to stop software changing under you must not change
//! under you either. So sluice never replaces itself on its own: the daily
//! check announces a new release once, the dashboard shows it, and
//! `sluice self-update` installs it when you ask. The download is checked
//! against the release's published checksums, the new binary is run before it
//! replaces the old one, and the old one is kept so
//! `sluice self-update --rollback` can put it back.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{LineageConfig, SelfUpdateConfig};
use crate::version::Evr;

/// The version of this binary.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// A published release, as GitHub's API describes it.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
}

impl Release {
    /// `v0.2.0` → `0.2.0`.
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    /// Whether this release is newer than the running binary.
    pub fn is_newer_than(&self, current: &str) -> bool {
        !self.draft && !self.prerelease && Evr::parse(self.version()) > Evr::parse(current)
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The first few lines of the release notes, for a preview.
    pub fn notes_excerpt(&self, lines: usize) -> Vec<String> {
        self.body
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(lines)
            .map(str::to_string)
            .collect()
    }
}

/// The release asset built for this CPU.
pub fn asset_name() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    Some(format!("sluice-{arch}-linux"))
}

fn api_url(cfg: &SelfUpdateConfig) -> String {
    cfg.api_url
        .clone()
        .unwrap_or_else(|| format!("https://api.github.com/repos/{}/releases/latest", cfg.repo))
}

/// The latest release. Background checks go through the lineage cache, so a
/// daily check and a dashboard refresh do not each spend an API request;
/// `fresh` asks GitHub now, for when you have explicitly asked.
pub fn latest(
    cfg: &SelfUpdateConfig,
    lineage: &LineageConfig,
    cache_dir: &Path,
    fresh: bool,
) -> Result<Release> {
    let lineage = LineageConfig {
        cache_ttl_hours: if fresh { 0 } else { lineage.cache_ttl_hours },
        ..lineage.clone()
    };
    let mut fetcher = crate::lineage::Fetcher::new(&lineage, cache_dir);
    let url = api_url(cfg);
    let Some(body) = fetcher.get(&url) else {
        if fetcher.warnings.iter().any(|w| w.contains("404")) {
            anyhow::bail!("no release of {} has been published yet", cfg.repo);
        }
        anyhow::bail!(
            "could not reach {url}{}",
            warnings_suffix(&fetcher.warnings)
        );
    };
    serde_json::from_str(&body).context("reading the release description")
}

fn warnings_suffix(w: &[String]) -> String {
    w.first().map(|w| format!(" ({w})")).unwrap_or_default()
}

/// The latest release if it is newer than this binary.
pub fn available(
    cfg: &SelfUpdateConfig,
    lineage: &LineageConfig,
    cache_dir: &Path,
) -> Option<Release> {
    if !cfg.check || lineage.offline {
        return None;
    }
    latest(cfg, lineage, cache_dir, false)
        .ok()
        .filter(|r| r.is_newer_than(CURRENT))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("sluice/", env!("CARGO_PKG_VERSION")))
        .build();
    let mut bytes = Vec::new();
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_reader()
        .take(256 * 1024 * 1024)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Check `bytes` against the `SHA256SUMS` line for `name`.
pub fn verify(bytes: &[u8], sums: &str, name: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|l| {
            let mut f = l.split_whitespace();
            let hash = f.next()?;
            let file = f.next()?.trim_start_matches('*');
            (file == name).then(|| hash.to_ascii_lowercase())
        })
        .with_context(|| format!("{name} is not listed in SHA256SUMS"))?;
    let actual = sha256_hex(bytes);
    anyhow::ensure!(
        actual == expected,
        "checksum mismatch for {name}: expected {expected}, got {actual}; the download was not used"
    );
    Ok(())
}

/// Download and verify this CPU's binary from `release`.
pub fn fetch_verified(release: &Release) -> Result<Vec<u8>> {
    let name = asset_name().context("there is no release build for this CPU")?;
    let binary = release
        .asset(&name)
        .with_context(|| format!("release {} has no {name}", release.tag_name))?;
    let sums = release
        .asset("SHA256SUMS")
        .with_context(|| format!("release {} publishes no SHA256SUMS", release.tag_name))?;
    let sums =
        String::from_utf8(download(&sums.browser_download_url)?).context("reading SHA256SUMS")?;
    let bytes = download(&binary.browser_download_url)?;
    verify(&bytes, &sums, &name)?;
    Ok(bytes)
}

/// Where the replaced binary is kept, for `--rollback`.
pub fn previous_path(state_dir: &Path) -> PathBuf {
    state_dir.join("previous-sluice")
}

/// Swap `new` in for the binary at `target`, keeping the old one at
/// `previous`. The new binary is run first: a download that does not start,
/// or reports another version, never replaces anything.
pub fn install(new: &[u8], expected_version: &str, target: &Path, previous: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = target.parent().context("the binary has no directory")?;
    let staged = dir.join(".sluice-update");
    std::fs::write(&staged, new).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;

    let out = std::process::Command::new(&staged)
        .arg("--version")
        .output();
    let reported = out
        .as_ref()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !reported.ends_with(expected_version) {
        let _ = std::fs::remove_file(&staged);
        anyhow::bail!(
            "the downloaded binary {} (expected {expected_version}); nothing was replaced",
            if reported.is_empty() {
                "did not run".to_string()
            } else {
                format!("reports `{reported}`")
            }
        );
    }

    if let Some(p) = previous.parent() {
        std::fs::create_dir_all(p)?;
    }
    if target.exists() {
        std::fs::copy(target, previous)
            .with_context(|| format!("keeping the old binary at {}", previous.display()))?;
    }
    std::fs::rename(&staged, target).with_context(|| format!("replacing {}", target.display()))?;
    Ok(())
}

/// Put the kept binary back.
pub fn rollback(target: &Path, previous: &Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    anyhow::ensure!(
        previous.exists(),
        "there is no previous version kept to roll back to"
    );
    let dir = target.parent().context("the binary has no directory")?;
    let staged = dir.join(".sluice-rollback");
    std::fs::copy(previous, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let version = std::process::Command::new(&staged)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    std::fs::rename(&staged, target)?;
    std::fs::remove_file(previous)?;
    Ok(version)
}

/// The binary to replace: the one running, unless it is a development build.
pub fn target_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    anyhow::ensure!(
        !exe.components().any(|c| c.as_os_str() == "target"),
        "{} is a development build; self-update replaces installed copies (see `sluice setup`)",
        exe.display()
    );
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        serde_json::from_str(&format!(
            r#"{{"tag_name":"{tag}","html_url":"https://example.org/r","published_at":"2026-10-01T00:00:00Z",
               "body":"Changes:\r\n\r\n- faster timeline\r\n- fixes","prerelease":false,"draft":false,
               "assets":[{{"name":"sluice-x86_64-linux","browser_download_url":"https://example.org/a"}},
                         {{"name":"SHA256SUMS","browser_download_url":"https://example.org/s"}}]}}"#
        ))
        .unwrap()
    }

    /// A tiny stand-in for GitHub: serves the release description, the
    /// binary and its checksums, then stops.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut stream = stream;
                match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        base
    }

    #[test]
    fn a_release_is_found_downloaded_verified_and_installed() {
        let Some(asset) = asset_name() else { return };
        let binary = script("9.9.9");
        let sums = format!("{}  {asset}\n", sha256_hex(&binary));
        // The description must name URLs on the server, so it is built once
        // the address is known: serve it from a second server.
        let files_base = serve(vec![
            ("/binary", binary.clone()),
            ("/SHA256SUMS", sums.into_bytes()),
        ]);
        let description = format!(
            r#"{{"tag_name":"v9.9.9","html_url":"","body":"notes","assets":[
                {{"name":"{asset}","browser_download_url":"{files_base}/binary"}},
                {{"name":"SHA256SUMS","browser_download_url":"{files_base}/SHA256SUMS"}}]}}"#
        );
        let api_base = serve(vec![("/latest", description.into_bytes())]);

        let dir = tempfile::tempdir().unwrap();
        let cfg = SelfUpdateConfig {
            api_url: Some(format!("{api_base}/latest")),
            ..Default::default()
        };
        let lineage = LineageConfig::default();
        let release = latest(&cfg, &lineage, dir.path(), true).unwrap();
        assert!(release.is_newer_than(CURRENT));
        assert!(available(&cfg, &lineage, dir.path()).is_some());

        let bytes = fetch_verified(&release).unwrap();
        let target = dir.path().join("sluice");
        std::fs::write(&target, script("0.1.0")).unwrap();
        install(
            &bytes,
            release.version(),
            &target,
            &dir.path().join("previous"),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&target).unwrap().contains("9.9.9"));
    }

    #[test]
    fn newer_releases_are_recognised() {
        assert!(release("v0.2.0").is_newer_than("0.1.0"));
        assert!(
            release("v0.10.0").is_newer_than("0.9.3"),
            "numeric, not lexical"
        );
        assert!(!release("v0.1.0").is_newer_than("0.1.0"));
        assert!(!release("v0.0.9").is_newer_than("0.1.0"));
        let mut pre = release("v0.3.0");
        pre.prerelease = true;
        assert!(!pre.is_newer_than("0.1.0"), "pre-releases are not offered");
    }

    #[test]
    fn release_notes_are_excerpted() {
        assert_eq!(
            release("v0.2.0").notes_excerpt(2),
            vec!["Changes:", "- faster timeline"]
        );
    }

    #[test]
    fn checksums_are_enforced() {
        let bytes = b"a binary";
        let sums = format!(
            "{}  sluice-x86_64-linux\n{}  other\n",
            sha256_hex(bytes),
            sha256_hex(b"x")
        );
        verify(bytes, &sums, "sluice-x86_64-linux").unwrap();
        assert!(verify(b"tampered", &sums, "sluice-x86_64-linux")
            .unwrap_err()
            .to_string()
            .contains("mismatch"));
        assert!(
            verify(bytes, &sums, "sluice-aarch64-linux").is_err(),
            "an unlisted file is refused"
        );
    }

    /// A stand-in binary that reports a version, like the real one.
    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'sluice {version}'\n").into_bytes()
    }

    #[test]
    fn install_keeps_the_old_binary_and_rollback_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sluice");
        let previous = dir.path().join("state/previous-sluice");
        std::fs::write(&target, script("0.1.0")).unwrap();

        install(&script("0.2.0"), "0.2.0", &target, &previous).unwrap();
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.2.0"));
        assert!(std::fs::read_to_string(&previous)
            .unwrap()
            .contains("0.1.0"));

        let restored = rollback(&target, &previous).unwrap();
        assert_eq!(restored, "sluice 0.1.0");
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.1.0"));
        assert!(!previous.exists());
    }

    #[test]
    fn a_binary_that_does_not_run_or_lies_about_its_version_replaces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sluice");
        let previous = dir.path().join("previous");
        std::fs::write(&target, script("0.1.0")).unwrap();

        let err = install(&script("0.1.5"), "0.2.0", &target, &previous).unwrap_err();
        assert!(err.to_string().contains("nothing was replaced"), "{err}");
        let err = install(b"not an executable", "0.2.0", &target, &previous).unwrap_err();
        assert!(err.to_string().contains("did not run"), "{err}");
        assert!(std::fs::read_to_string(&target).unwrap().contains("0.1.0"));
        assert!(!previous.exists());
    }
}
