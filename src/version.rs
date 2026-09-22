//! RPM version handling: `rpmvercmp` ordering and series extraction.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// An RPM epoch:version-release triple.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evr {
    pub epoch: u32,
    pub version: String,
    pub release: String,
}

impl Evr {
    /// Parse `[epoch:]version[-release]`, the form zypper and rpm both print.
    pub fn parse(s: &str) -> Self {
        let (epoch, rest) = match s.split_once(':') {
            Some((e, r)) if e.chars().all(|c| c.is_ascii_digit()) && !e.is_empty() => {
                (e.parse().unwrap_or(0), r)
            }
            _ => (0, s),
        };
        // The release is everything after the LAST '-': RPM versions themselves
        // may not contain '-', but being explicit avoids surprises.
        let (version, release) = match rest.rsplit_once('-') {
            Some((v, r)) => (v.to_string(), r.to_string()),
            None => (rest.to_string(), String::new()),
        };
        Evr {
            epoch,
            version,
            release,
        }
    }

    /// The version without its release, e.g. `7.2.6` from `7.2.6-1.1`.
    pub fn upstream(&self) -> &str {
        &self.version
    }

    /// Extract the series key using `regex`'s first capture group.
    pub fn series(&self, regex: &Regex) -> Option<String> {
        regex
            .captures(&self.version)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    }

    /// The trailing point-release number, e.g. `6` from `7.2.6`. `None` when the
    /// version has no component beyond the series (`7.2` -> `None`).
    pub fn point_release(&self, regex: &Regex) -> Option<u32> {
        let series = self.series(regex)?;
        let tail = self.version.strip_prefix(&series)?.trim_start_matches('.');
        tail.split(|c: char| !c.is_ascii_digit())
            .next()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
    }
}

impl fmt::Display for Evr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}:", self.epoch)?;
        }
        f.write_str(&self.version)?;
        if !self.release.is_empty() {
            write!(f, "-{}", self.release)?;
        }
        Ok(())
    }
}

impl Ord for Evr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| rpmvercmp(&self.version, &other.version))
            .then_with(|| rpmvercmp(&self.release, &other.release))
    }
}

impl PartialOrd for Evr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A faithful port of rpm's `rpmvercmp`.
///
/// Segments of digits and of letters are compared pairwise; digit segments
/// always outrank alphabetic ones; a tilde sorts before anything, including the
/// empty string; a caret sorts before everything except the end of string.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);

    loop {
        // Skip separators, which are any non-alphanumeric byte other than ~ and ^.
        while i < a.len() && !is_alnum(a[i]) && a[i] != b'~' && a[i] != b'^' {
            i += 1;
        }
        while j < b.len() && !is_alnum(b[j]) && b[j] != b'~' && b[j] != b'^' {
            j += 1;
        }

        // Tilde sorts before everything, so a side that has one loses.
        let a_tilde = i < a.len() && a[i] == b'~';
        let b_tilde = j < b.len() && b[j] == b'~';
        if a_tilde || b_tilde {
            match (a_tilde, b_tilde) {
                (true, true) => {
                    i += 1;
                    j += 1;
                    continue;
                }
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                (false, false) => unreachable!(),
            }
        }

        // Caret sorts before everything except the end of the string.
        let a_caret = i < a.len() && a[i] == b'^';
        let b_caret = j < b.len() && b[j] == b'^';
        if a_caret || b_caret {
            match (a_caret, b_caret) {
                (true, true) => {
                    i += 1;
                    j += 1;
                    continue;
                }
                // A caret loses to a remaining segment but beats end-of-string.
                (true, false) => {
                    return if j >= b.len() {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, true) => {
                    return if i >= a.len() {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, false) => unreachable!(),
            }
        }

        if i >= a.len() || j >= b.len() {
            break;
        }

        let numeric = a[i].is_ascii_digit();
        let a_seg = take_segment(a, &mut i, numeric);
        let b_seg = take_segment(b, &mut j, numeric);

        // A numeric segment on one side and an alphabetic one on the other:
        // numeric wins. `b_seg` is empty exactly when the kinds disagree.
        if b_seg.is_empty() {
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        let ord = if numeric {
            let a_trim = a_seg.trim_start_matches('0');
            let b_trim = b_seg.trim_start_matches('0');
            a_trim
                .len()
                .cmp(&b_trim.len())
                .then_with(|| a_trim.cmp(b_trim))
        } else {
            a_seg.cmp(&b_seg)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }

    // Whichever string still has content is the newer one.
    match (i >= a.len(), j >= b.len()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => unreachable!(),
    }
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// Consume the run of digits (or of letters) starting at `*idx`.
fn take_segment(s: &[u8], idx: &mut usize, numeric: bool) -> String {
    let start = *idx;
    while *idx < s.len()
        && (if numeric {
            s[*idx].is_ascii_digit()
        } else {
            s[*idx].is_ascii_alphabetic()
        })
    {
        *idx += 1;
    }
    String::from_utf8_lossy(&s[start..*idx]).into_owned()
}

/// Compile a component's series regex, checking it has a capture group.
pub fn series_regex(pattern: &str) -> Result<Regex> {
    let re = Regex::new(pattern).with_context(|| format!("invalid series regex `{pattern}`"))?;
    anyhow::ensure!(
        re.captures_len() >= 2,
        "series regex `{pattern}` needs a capture group for the series key"
    );
    Ok(re)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> Ordering {
        rpmvercmp(a, b)
    }

    #[test]
    fn rpmvercmp_matches_rpm_test_suite() {
        assert_eq!(cmp("1.0", "1.0"), Ordering::Equal);
        assert_eq!(cmp("1.0", "2.0"), Ordering::Less);
        assert_eq!(cmp("2.0", "1.0"), Ordering::Greater);
        assert_eq!(cmp("2.0.1", "2.0.1"), Ordering::Equal);
        assert_eq!(cmp("2.0", "2.0.1"), Ordering::Less);
        assert_eq!(cmp("2.0.1a", "2.0.1"), Ordering::Greater);
        assert_eq!(cmp("1.0", "1.0a"), Ordering::Less);
        assert_eq!(cmp("10", "9"), Ordering::Greater);
        assert_eq!(cmp("1.05", "1.5"), Ordering::Equal);
        assert_eq!(cmp("xyz10", "xyz10.1"), Ordering::Less);
        assert_eq!(cmp("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(cmp("1.0~rc1", "1.0~rc2"), Ordering::Less);
        assert_eq!(cmp("1.0^", "1.0"), Ordering::Greater);
        assert_eq!(cmp("1.0^", "1.0.1"), Ordering::Less);
    }

    #[test]
    fn kernel_versions_order_correctly() {
        let a = Evr::parse("7.2.0-1.1");
        let b = Evr::parse("7.2.6-1.1");
        let c = Evr::parse("7.3.0-1.1");
        assert!(a < b, "7.2.0 should precede 7.2.6");
        assert!(b < c, "7.2.6 should precede 7.3.0");
    }

    #[test]
    fn evr_parses_epoch_and_release() {
        let e = Evr::parse("2:1.4.0-3.2");
        assert_eq!(e.epoch, 2);
        assert_eq!(e.version, "1.4.0");
        assert_eq!(e.release, "3.2");
        assert_eq!(e.to_string(), "2:1.4.0-3.2");

        let bare = Evr::parse("20260829");
        assert_eq!(bare.epoch, 0);
        assert_eq!(bare.version, "20260829");
        assert_eq!(bare.release, "");
    }

    #[test]
    fn series_and_point_release_extraction() {
        let re = series_regex(r"^(\d+\.\d+)").unwrap();
        let v = Evr::parse("7.2.6-1.1");
        assert_eq!(v.series(&re).as_deref(), Some("7.2"));
        assert_eq!(v.point_release(&re), Some(6));

        // A .0 release and a series-only version are different things.
        assert_eq!(Evr::parse("7.3.0-1.1").point_release(&re), Some(0));
        assert_eq!(Evr::parse("7.3-1.1").point_release(&re), None);
    }

    #[test]
    fn date_versioned_firmware_has_no_meaningful_series() {
        let re = series_regex(r"^(\d+\.\d+)").unwrap();
        assert_eq!(Evr::parse("20260829-1.1").series(&re), None);
    }
}
