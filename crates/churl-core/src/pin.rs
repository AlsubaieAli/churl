//! Optional workspace version pinning via a `.churl-version` file.
//!
//! A workspace may carry a `.churl-version` file at its root naming a single
//! exact version string (nvmrc-style). It is **advisory**: when present and the
//! running binary's version does not match, churl warns on stderr and runs
//! anyway — it never refuses, and it never switches or acquires a version.
//! Absent ⇒ no-op (the installed version is used).
//!
//! This module is the pure half: discovery (find the file at the workspace
//! root), parse (trim to a single line), and the version compare. The warning
//! *display* lives in the `churl` binary and is surfaced once at workspace load.

use std::path::{Path, PathBuf};

/// The reserved filename churl reads a version pin from, at a workspace root.
/// A dotfile so it stays out of the way; absent means "no pin".
pub const PIN_FILENAME: &str = ".churl-version";

/// Error reading a `.churl-version` file. A *missing* file is not an error
/// (it means "no pin") — [`read_pin`] returns `Ok(None)` for that case; this
/// error covers only an I/O failure on a file that does exist.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PinError {
    /// The `.churl-version` file exists but could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        /// Path of the pin file.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// The outcome of comparing the running version against a workspace pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinCheck {
    /// The running version satisfies the pin — no warning.
    Satisfied,
    /// The running version does not satisfy the pin — the binary warns and
    /// runs anyway, never refusing.
    Mismatch {
        /// The version requested by `.churl-version`.
        pinned: String,
        /// The version of the running binary.
        running: String,
    },
}

/// Returns the path of the `.churl-version` pin file at `workspace_root` when it
/// exists, else `None`. Discovery is root-only (nvmrc-style) — churl does not
/// walk parent directories.
pub fn discover_pin(workspace_root: &Path) -> Option<PathBuf> {
    let path = workspace_root.join(PIN_FILENAME);
    path.is_file().then_some(path)
}

/// Reads and parses a `.churl-version` file at `path`.
///
/// Returns the trimmed pin string on success. A file that is missing, empty, or
/// whitespace-only yields `Ok(None)` — treated exactly like an absent pin (a
/// no-op), never a crash. Only the first non-empty line is honoured (trailing
/// lines/comments are ignored), matching nvmrc's lenient single-value form.
pub fn read_pin(path: &Path) -> Result<Option<String>, PinError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(PinError::Read {
                path: path.to_owned(),
                source: err,
            });
        }
    };
    Ok(parse_pin(&text))
}

/// Parses the textual contents of a `.churl-version` file: the first non-empty
/// trimmed line, or `None` when the file holds no version token. Pure — split
/// out from [`read_pin`] so it is unit-testable without touching the filesystem.
pub fn parse_pin(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Compares the running binary version against a workspace `pinned` version.
///
/// A single optional leading `v` is stripped from each side first (so a
/// `v0.2.0` pin matches a `0.2.0` build — matching how release tags are
/// normalised). Semver-aware when both then parse: it compares the precedence
/// fields (major.minor.patch + pre-release), ignoring build metadata (`+meta`
/// carries no precedence per the semver spec, so `1.2.3+ci` satisfies `1.2.3`).
/// When either side is not valid semver, it falls back to exact string equality
/// (nvmrc-style), so a non-semver tag still compares sensibly and never panics.
pub fn check_pin(pinned: &str, running: &str) -> PinCheck {
    let pinned = pinned.trim();
    let running = running.trim();
    let pinned_v = strip_v(pinned);
    let running_v = strip_v(running);
    let satisfied = match (
        semver::Version::parse(pinned_v),
        semver::Version::parse(running_v),
    ) {
        (Ok(want), Ok(have)) => {
            want.major == have.major
                && want.minor == have.minor
                && want.patch == have.patch
                && want.pre == have.pre
        }
        // Either side is not semver → exact string match on the v-stripped forms
        // (nvmrc-style), so `v0.2.0` still equals `0.2.0` in the fallback too.
        _ => pinned_v == running_v,
    };
    if satisfied {
        PinCheck::Satisfied
    } else {
        PinCheck::Mismatch {
            pinned: pinned.to_owned(),
            running: running.to_owned(),
        }
    }
}

/// Strips a single optional leading `v` from a version string (`v0.2.0` →
/// `0.2.0`), leaving everything else untouched.
fn strip_v(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pin_takes_first_non_empty_trimmed_line() {
        assert_eq!(parse_pin("  0.2.0  \n"), Some("0.2.0".to_owned()));
        assert_eq!(
            parse_pin("\n\n  1.0.0\nignored\n"),
            Some("1.0.0".to_owned())
        );
    }

    #[test]
    fn parse_pin_empty_or_whitespace_is_none() {
        assert_eq!(parse_pin(""), None);
        assert_eq!(parse_pin("   \n\t\n"), None);
    }

    #[test]
    fn check_pin_matches_is_satisfied() {
        assert_eq!(check_pin("0.2.0", "0.2.0"), PinCheck::Satisfied);
    }

    #[test]
    fn check_pin_semver_ignores_build_metadata() {
        assert_eq!(check_pin("0.2.0", "0.2.0+ci.7"), PinCheck::Satisfied);
    }

    #[test]
    fn check_pin_strips_leading_v_no_spurious_warn() {
        // A `v`-prefixed pin (or build) must not warn against the bare version.
        assert_eq!(check_pin("v0.2.0", "0.2.0"), PinCheck::Satisfied);
        assert_eq!(check_pin("0.2.0", "v0.2.0"), PinCheck::Satisfied);
        assert_eq!(check_pin("v0.2.0", "v0.2.0"), PinCheck::Satisfied);
        // A genuine mismatch still warns, preserving the user's literal strings.
        assert_eq!(
            check_pin("v0.3.0", "0.2.0"),
            PinCheck::Mismatch {
                pinned: "v0.3.0".to_owned(),
                running: "0.2.0".to_owned(),
            }
        );
    }

    #[test]
    fn check_pin_differs_is_mismatch() {
        assert_eq!(
            check_pin("0.3.0", "0.2.0"),
            PinCheck::Mismatch {
                pinned: "0.3.0".to_owned(),
                running: "0.2.0".to_owned(),
            }
        );
    }

    #[test]
    fn check_pin_non_semver_falls_back_to_exact_string() {
        assert_eq!(check_pin("nightly", "nightly"), PinCheck::Satisfied);
        assert_eq!(
            check_pin("nightly", "0.2.0"),
            PinCheck::Mismatch {
                pinned: "nightly".to_owned(),
                running: "0.2.0".to_owned(),
            }
        );
    }

    #[test]
    fn discover_pin_present_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discover_pin(dir.path()), None);
        let pin = dir.path().join(PIN_FILENAME);
        std::fs::write(&pin, "0.2.0\n").unwrap();
        assert_eq!(discover_pin(dir.path()), Some(pin));
    }

    #[test]
    fn read_pin_present_matches_and_differs() {
        let dir = tempfile::tempdir().unwrap();
        let pin = dir.path().join(PIN_FILENAME);
        std::fs::write(&pin, "0.2.0\n").unwrap();
        assert_eq!(read_pin(&pin).unwrap(), Some("0.2.0".to_owned()));
    }

    #[test]
    fn read_pin_absent_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(PIN_FILENAME);
        assert_eq!(read_pin(&missing).unwrap(), None);
    }

    #[test]
    fn read_pin_malformed_whitespace_is_none_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let pin = dir.path().join(PIN_FILENAME);
        std::fs::write(&pin, "   \n\t\n").unwrap();
        assert_eq!(read_pin(&pin).unwrap(), None);
    }
}
