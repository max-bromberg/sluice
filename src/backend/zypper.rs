//! zypper / rpm backend for openSUSE.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::{CmpOp, InstalledPkg, LockSpec, PackageBackend, Pkg, PkgStatus, Transaction};
use crate::exec::{Cmd, Runner};
use crate::version::Evr;

/// zypper exit codes that mean "worked, but something is pending".
/// 0 ok, 100 updates available, 101 security updates available,
/// 102 reboot required, 103 restart of the package manager needed.
const SOFT_OK: &[i32] = &[0, 100, 101, 102, 103];

pub struct Zypper {
    /// Path to the zypper binary; configurable mainly so tests and containers
    /// can point at a wrapper.
    program: String,
    locks_file: PathBuf,
}

impl Default for Zypper {
    fn default() -> Self {
        Zypper {
            program: "zypper".into(),
            locks_file: "/etc/zypp/locks".into(),
        }
    }
}

impl Zypper {
    pub fn new(program: impl Into<String>, locks_file: impl Into<PathBuf>) -> Self {
        Zypper {
            program: program.into(),
            locks_file: locks_file.into(),
        }
    }

    fn base(&self, mutating: bool) -> Cmd {
        let c = if mutating {
            Cmd::mutate(&self.program)
        } else {
            Cmd::read(&self.program)
        };
        c.arg("--non-interactive")
    }
}

impl PackageBackend for Zypper {
    fn name(&self) -> &'static str {
        "zypper"
    }

    fn refresh(&self, r: &mut Runner) -> Result<()> {
        // A refresh writes only to the metadata cache, so it runs even under
        // --dry-run: policy evaluation is worthless against stale metadata.
        let out = r.run(&self.base(false).arg("refresh"))?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status),
            "zypper refresh failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        Ok(())
    }

    fn query(&self, r: &mut Runner, pattern: &str) -> Result<Vec<Pkg>> {
        self.query_many(r, &[pattern.to_string()])
    }

    fn query_many(&self, r: &mut Runner, patterns: &[String]) -> Result<Vec<Pkg>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        // `--xmlout search -s` is the one zypper query that emits genuinely
        // structured data. `info` merely wraps its human-readable text in a
        // <message> element, so it is never parsed here.
        let cmd = self
            .base(false)
            .arg("--xmlout")
            .arg("--no-refresh")
            .arg("search")
            .arg("-s")
            .arg("--type=package")
            .args(patterns);
        let out = r.run(&cmd)?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status) || out.status == 104, // 104: nothing found
            "zypper search failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        parse_solvables(&out.stdout)
    }

    fn installed_details(&self, r: &mut Runner, names: &[String]) -> Result<Vec<InstalledPkg>> {
        let query = if names.is_empty() {
            Cmd::read("rpm").arg("-qa")
        } else {
            Cmd::read("rpm").arg("-q").args(names)
        };
        let out = r.run(&query.arg("--qf").arg(
            "%{NAME}\\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\\t%{SOURCERPM}\\t%{INSTALLTIME}\\n",
        ))?;
        // `rpm -q` exits non-zero if any named package is missing, but still
        // prints the ones that are installed.
        if names.is_empty() {
            out.require_ok("rpm -qa")?;
        }
        Ok(parse_installed(&out.stdout))
    }

    fn locks(&self, _r: &mut Runner) -> Result<Vec<LockSpec>> {
        // The locks file is parsed directly rather than scraped from
        // `zypper locks`, whose table truncates long specs and drops the
        // version condition entirely.
        match std::fs::read_to_string(&self.locks_file) {
            Ok(text) => parse_locks_file(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.locks_file.display())),
        }
    }

    fn add_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()> {
        let mut cmd = self.base(true).arg("addlock");
        if let Some(c) = &spec.comment {
            cmd = cmd.arg("--comment").arg(c);
        }
        let out = r.run(&cmd.arg(spec.spec_string()))?;
        out.require_ok("zypper addlock")?;
        Ok(())
    }

    fn remove_lock(&self, r: &mut Runner, spec: &LockSpec) -> Result<()> {
        let out = r.run(&self.base(true).arg("removelock").arg(spec.spec_string()))?;
        out.require_ok("zypper removelock")?;
        Ok(())
    }

    fn dist_upgrade(&self, r: &mut Runner, extra_args: &[String]) -> Result<Transaction> {
        let cmd = self.base(true).arg("dup").args(extra_args);
        let out = r.run(&cmd)?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status),
            "zypper dup failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        Ok(Transaction {
            installed: Vec::new(),
            removed: Vec::new(),
            output: out.stdout,
        })
    }

    fn install_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        if pkgs.is_empty() {
            return Ok(Transaction::default());
        }
        // --oldpackage allows a downgrade, which is what a rollback to a
        // vaulted kernel is. Every member of the family goes in one
        // transaction so the set can never end up half-moved.
        let cmd = self
            .base(true)
            .arg("install")
            .arg("--oldpackage")
            .arg("--no-recommends")
            .args(pkgs.iter().map(Pkg::exact_spec));
        let out = r.run(&cmd)?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status),
            "zypper install failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        Ok(Transaction {
            installed: pkgs.to_vec(),
            removed: Vec::new(),
            output: out.stdout,
        })
    }

    fn fetch_rpms(&self, r: &mut Runner, pkgs: &[Pkg], dest: &Path) -> Result<Vec<PathBuf>> {
        if pkgs.is_empty() {
            return Ok(Vec::new());
        }
        if r.dry_run() {
            // Recorded in the transcript and the log, but nothing is written.
            r.run(
                &self
                    .base(true)
                    .arg("--pkg-cache-dir")
                    .arg(dest)
                    .arg("download")
                    .args(pkgs.iter().map(Pkg::exact_spec)),
            )?;
            return Ok(Vec::new());
        }
        std::fs::create_dir_all(dest)
            .with_context(|| format!("creating vault directory {}", dest.display()))?;
        // `download`, not `install --download-only`: the latter does nothing
        // for a version that is already installed, which is exactly the version
        // `mark-good` wants kept. `download` fetches regardless, and needs no
        // root. Files land in `<dest>/<repo alias>/<arch>/`.
        let cmd = self
            .base(true)
            .arg("--no-refresh")
            .arg("--pkg-cache-dir")
            .arg(dest)
            .arg("download")
            .args(pkgs.iter().map(Pkg::exact_spec));
        let out = r.run(&cmd)?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status),
            "downloading RPMs failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        if r.dry_run() {
            return Ok(Vec::new());
        }

        let files = find_rpms(dest);
        let missing: Vec<String> = pkgs
            .iter()
            .filter(|p| {
                let name = format!("{}.rpm", p.nevra());
                !files
                    .iter()
                    .any(|f| f.file_name().is_some_and(|n| n == name.as_str()))
            })
            .map(Pkg::nevra)
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "the repositories did not supply: {}",
            missing.join(", ")
        );
        Ok(files)
    }

    fn remove_exact(&self, r: &mut Runner, pkgs: &[Pkg]) -> Result<Transaction> {
        if pkgs.is_empty() {
            return Ok(Transaction::default());
        }
        let cmd = self
            .base(true)
            .arg("remove")
            .args(pkgs.iter().map(Pkg::exact_spec));
        let out = r.run(&cmd)?;
        anyhow::ensure!(
            SOFT_OK.contains(&out.status),
            "zypper remove failed ({}):\n{}",
            out.status,
            out.stderr.trim()
        );
        Ok(Transaction {
            installed: Vec::new(),
            removed: pkgs.to_vec(),
            output: out.stdout,
        })
    }

    fn changelog(&self, r: &mut Runner, pkg: &Pkg) -> Result<Option<String>> {
        let out = r.run(
            &Cmd::read("rpm")
                .arg("-q")
                .arg("--changelog")
                .arg(pkg.nevra()),
        )?;
        Ok(out.ok().then_some(out.stdout))
    }

    fn file_changelog(&self, r: &mut Runner, path: &Path) -> Result<Option<String>> {
        let out = r.run(&Cmd::read("rpm").arg("-qp").arg("--changelog").arg(path))?;
        Ok(out.ok().then_some(out.stdout))
    }

    fn ensure_local_repo(
        &self,
        r: &mut Runner,
        alias: &str,
        dir: &Path,
        priority: u32,
    ) -> Result<bool> {
        if r.run(&self.base(false).arg("lr").arg(alias))?.ok() {
            return Ok(false);
        }
        r.run(
            &self
                .base(true)
                .arg("addrepo")
                .arg("--type")
                .arg("plaindir")
                .arg("--priority")
                .arg(priority.to_string())
                .arg("--no-gpgcheck")
                .arg("--keep-packages")
                .arg(format!("dir://{}", dir.display()))
                .arg(alias),
        )?
        .require_ok(&format!("registering the {alias} repository"))?;
        Ok(true)
    }
}

/// Parse `<solvable .../>` elements out of `zypper --xmlout search -s`.
fn parse_solvables(xml: &str) -> Result<Vec<Pkg>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"solvable" => {
                let mut name = None;
                let mut edition = None;
                let mut arch = String::new();
                let mut repo = String::new();
                let mut status = String::new();
                let mut kind = String::from("package");

                for attr in e.attributes().flatten() {
                    let value = attr.unescape_value().unwrap_or_default().into_owned();
                    match attr.key.as_ref() {
                        b"name" => name = Some(value),
                        b"edition" => edition = Some(value),
                        b"arch" => arch = value,
                        b"repository" => repo = value,
                        b"status" => status = value,
                        b"kind" => kind = value,
                        _ => {}
                    }
                }

                if kind != "package" {
                    continue;
                }
                let (Some(name), Some(edition)) = (name, edition) else {
                    continue;
                };
                out.push(Pkg {
                    name,
                    evr: Evr::parse(&edition),
                    arch,
                    repo,
                    // zypper reports "installed" for the version on the system and
                    // "other-version"/"not-installed" for the rest.
                    status: if status == "installed" {
                        PkgStatus::Installed
                    } else {
                        PkgStatus::Available
                    },
                    source: None,
                });
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("parsing zypper XML: {e}")),
            _ => {}
        }
    }
    Ok(out)
}

/// Parse `NAME\tEPOCH:VERSION-RELEASE\tSOURCERPM\tINSTALLTIME` lines. The
/// source package name is the SOURCERPM file name without
/// `-<version>-<release>.src.rpm`.
fn parse_installed(text: &str) -> Vec<InstalledPkg> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let name = f.next()?.to_string();
            let evr = Evr::parse(f.next()?);
            let source = f.next().and_then(|srpm| {
                let stem = srpm
                    .strip_suffix(".src.rpm")
                    .or_else(|| srpm.strip_suffix(".nosrc.rpm"))?;
                let mut parts = stem.rsplitn(3, '-');
                let (_release, _version) = (parts.next()?, parts.next()?);
                parts.next().map(str::to_string)
            });
            let installed_at = f
                .next()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0));
            Some(InstalledPkg {
                name,
                evr,
                source,
                installed_at,
            })
        })
        .collect()
}

/// Parse `/etc/zypp/locks`, whose records are blank-line separated
/// `attribute: value` blocks. See locks(5).
fn parse_locks_file(text: &str) -> Result<Vec<LockSpec>> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name: Option<String> = None;
        let mut version: Option<String> = None;
        let mut comment: Option<String> = None;

        for line in block.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "solvable_name" => name = Some(value),
                "version" => version = Some(value),
                "comment" => comment = Some(value),
                _ => {}
            }
        }

        let Some(name) = name else { continue };
        let bound = version.as_deref().and_then(parse_version_condition);
        out.push(LockSpec {
            name,
            bound,
            comment,
        });
    }
    Ok(out)
}

/// `">= 7.3"`, `"7.2.6-1.1"` (implicit `==`) -> a comparison bound.
fn parse_version_condition(s: &str) -> Option<(CmpOp, Evr)> {
    let s = s.trim();
    // Longest operators first, so ">=" is not read as ">".
    for (token, op) in [
        (">=", CmpOp::Ge),
        ("<=", CmpOp::Le),
        ("==", CmpOp::Eq),
        ("!=", CmpOp::Ne),
        (">", CmpOp::Gt),
        ("<", CmpOp::Lt),
        ("=", CmpOp::Eq),
    ] {
        if let Some(rest) = s.strip_prefix(token) {
            let rest = rest.trim();
            return (!rest.is_empty()).then(|| (op, Evr::parse(rest)));
        }
    }
    (!s.is_empty()).then(|| (CmpOp::Eq, Evr::parse(s)))
}

fn find_rpms(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rpm") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rpm -qa` lines in the shape Tumbleweed produces: Mesa is built from two
    /// sources at different releases, and Mesa-demo is not Mesa.
    #[test]
    fn parses_source_packages() {
        let text = "Mesa\t0:26.2.2-2.1\tMesa-26.2.2-2.1.src.rpm\t1758240701\n\
                    libvulkan_radeon\t0:26.2.2-2.2\tMesa-drivers-26.2.2-2.2.src.rpm\t1758240701\n\
                    Mesa-demo-x\t0:9.0.0-7.5\tMesa-demo-9.0.0-7.5.src.rpm\t1758240701\n\
                    kernel-default\t0:7.2.0-1.1\tkernel-default-7.2.0-1.1.nosrc.rpm\t1756311145\n\
                    gpg-pubkey\t0:29b700a4-62b07e22\t(none)\t1700000000\n";
        let pkgs = parse_installed(text);
        assert_eq!(pkgs.len(), 5);
        assert_eq!(pkgs[1].name, "libvulkan_radeon");
        assert_eq!(pkgs[1].evr.to_string(), "26.2.2-2.2");
        assert_eq!(pkgs[1].source.as_deref(), Some("Mesa-drivers"));
        assert_eq!(pkgs[2].source.as_deref(), Some("Mesa-demo"));
        assert_eq!(pkgs[3].source.as_deref(), Some("kernel-default"));
        assert_eq!(
            pkgs[4].source, None,
            "a package with no source RPM has no source"
        );
        assert_eq!(
            pkgs[3].installed_at.unwrap().date_naive().to_string(),
            "2025-08-27"
        );
    }

    // Captured verbatim from `zypper --xmlout search -s --match-exact kernel-default`
    // on openSUSE Tumbleweed.
    const SEARCH_XML: &str = r#"<?xml version='1.0'?>
<stream>
<message type="info">Loading repository data...</message>
<search-result version="0.0">
<solvable-list>
<solvable status="installed" name="kernel-default" kind="package" edition="7.2.0-1.1" arch="x86_64" repository="(System Packages)"/>
<solvable status="installed" name="kernel-default" kind="package" edition="7.0.12-1.1" arch="x86_64" repository="(System Packages)"/>
<solvable status="other-version" name="kernel-default" kind="package" edition="7.2.6-1.1" arch="x86_64" repository="openSUSE-Tumbleweed-Oss"/>
<solvable status="not-installed" name="kernel-default" kind="srcpackage" edition="7.2.6-1.1" arch="nosrc" repository="openSUSE-Tumbleweed-Source"/>
</solvable-list>
</search-result>
</stream>"#;

    #[test]
    fn parses_search_output_and_skips_non_packages() {
        let pkgs = parse_solvables(SEARCH_XML).unwrap();
        assert_eq!(pkgs.len(), 3, "the srcpackage solvable must be skipped");

        let installed: Vec<_> = pkgs.iter().filter(|p| p.installed()).collect();
        assert_eq!(installed.len(), 2);

        let candidate = pkgs.iter().find(|p| !p.installed()).unwrap();
        assert_eq!(candidate.evr.to_string(), "7.2.6-1.1");
        assert_eq!(candidate.repo, "openSUSE-Tumbleweed-Oss");
        assert_eq!(candidate.exact_spec(), "kernel-default=7.2.6-1.1");
    }

    #[test]
    fn empty_search_result_is_not_an_error() {
        let pkgs = parse_solvables("<?xml version='1.0'?>\n<stream>\n</stream>").unwrap();
        assert!(pkgs.is_empty());
    }

    #[test]
    fn parses_blanket_lock_as_written_by_zypper_addlock() {
        // Exactly the file produced by `zypper addlock kernel-default`.
        let text = "\ntype: package\nmatch_type: glob\ncase_sensitive: on\nsolvable_name: kernel-default\n";
        let locks = parse_locks_file(text).unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].name, "kernel-default");
        assert!(locks[0].is_blanket());
    }

    #[test]
    fn parses_version_conditional_locks() {
        let text = "\ntype: package\nsolvable_name: kernel-default\nversion: >= 7.3\ncomment: sluice series gate\n\ntype: package\nsolvable_name: Mesa\nversion: 26.3\n";
        let locks = parse_locks_file(text).unwrap();
        assert_eq!(locks.len(), 2);

        let (op, evr) = locks[0].bound.clone().unwrap();
        assert_eq!(op, CmpOp::Ge);
        assert_eq!(evr.version, "7.3");
        assert_eq!(locks[0].spec_string(), "kernel-default >= 7.3");
        assert_eq!(locks[0].comment.as_deref(), Some("sluice series gate"));

        // A bare version means equality.
        assert_eq!(locks[1].bound.clone().unwrap().0, CmpOp::Eq);
    }

    #[test]
    fn version_condition_prefers_the_two_character_operator() {
        assert_eq!(parse_version_condition(">= 7.3").unwrap().0, CmpOp::Ge);
        assert_eq!(parse_version_condition("> 7.3").unwrap().0, CmpOp::Gt);
        assert_eq!(parse_version_condition("<=7.3").unwrap().0, CmpOp::Le);
        assert!(parse_version_condition(">=").is_none());
    }
}
