//! Cross-platform path-behaviour invariants for churl-core.
//!
//! These run under the CI OS matrix (Linux/macOS/Windows). The point is to
//! pin behaviour that genuinely differs across platforms — path separators,
//! `dirs` config/data resolution, and the slug that becomes an on-disk
//! filename — using `std::path` component logic rather than string
//! comparison, so an assertion is not accidentally separator-specific.

use std::path::{Component, Path};

use churl_core::config::global_config_path;
use churl_core::history::default_state_path;
use churl_core::persistence::slug_of;

/// The last two components of the global config path are always `churl` then
/// `config.toml`, regardless of the platform's separator (`/` vs `\`). Checked
/// via `Path::components`, not string matching, so it holds on Windows too.
#[test]
fn global_config_path_ends_with_churl_config_toml() {
    let Some(path) = global_config_path() else {
        // No platform config dir (e.g. a stripped CI env with no HOME): nothing
        // to assert, but the call must not panic.
        return;
    };
    let tail: Vec<_> = path.components().rev().take(2).collect();
    assert_eq!(
        tail,
        vec![
            Component::Normal("config.toml".as_ref()),
            Component::Normal("churl".as_ref()),
        ],
        "config path must end with churl{}config.toml, got {}",
        std::path::MAIN_SEPARATOR,
        path.display()
    );
    // The file name is exactly config.toml on every platform.
    assert_eq!(path.file_name().unwrap(), "config.toml");
}

/// The default state DB path always ends with `churl` then `state.sqlite`.
#[test]
fn default_state_path_ends_with_churl_state_sqlite() {
    let Some(path) = default_state_path() else {
        return;
    };
    let tail: Vec<_> = path.components().rev().take(2).collect();
    assert_eq!(
        tail,
        vec![
            Component::Normal("state.sqlite".as_ref()),
            Component::Normal("churl".as_ref()),
        ],
        "state path must end with churl{}state.sqlite, got {}",
        std::path::MAIN_SEPARATOR,
        path.display()
    );
    assert_eq!(path.file_name().unwrap(), "state.sqlite");
}

/// Both platform paths, when resolvable, are absolute — `dirs` never hands back
/// a relative directory, so churl never resolves config/state relative to an
/// arbitrary CWD.
#[test]
fn platform_paths_are_absolute_when_present() {
    if let Some(p) = global_config_path() {
        assert!(
            p.is_absolute(),
            "config path must be absolute: {}",
            p.display()
        );
    }
    if let Some(p) = default_state_path() {
        assert!(
            p.is_absolute(),
            "state path must be absolute: {}",
            p.display()
        );
    }
}

/// A slug is a single path component: it never contains a path separator of
/// *either* platform, so it can be `join`ed as one filename segment without
/// silently creating a subdirectory on Windows (`\`) or Unix (`/`).
#[test]
fn slug_is_a_single_path_segment() {
    // Names that embed separators, dots, and mixed case — the slugifier must
    // collapse every non-alphanumeric run to a single '-'.
    for name in [
        "a/b/c",
        "a\\b\\c",
        "../etc/passwd",
        "C:\\Windows\\System32",
        "My Cool Request!!!",
        "..",
        "  spaced  ",
    ] {
        let slug = slug_of(name);
        assert!(
            !slug.contains('/') && !slug.contains('\\'),
            "slug {slug:?} (from {name:?}) must contain no path separator"
        );
        // A slug must be exactly one Normal component — never a parent-dir,
        // root, or prefix — so joining it can never escape a directory.
        let comps: Vec<_> = Path::new(&slug).components().collect();
        assert_eq!(comps.len(), 1, "slug {slug:?} must be one component");
        assert!(
            matches!(comps[0], Component::Normal(_)),
            "slug {slug:?} must be a Normal component, not {:?}",
            comps[0]
        );
    }
}

/// A name that slugifies to nothing (all separators/punctuation) falls back to
/// the reserved `unnamed` stem rather than an empty string, so the on-disk file
/// always has a name on every platform.
#[test]
fn empty_slug_falls_back_to_unnamed() {
    assert_eq!(slug_of("///"), "unnamed");
    assert_eq!(slug_of("\\\\"), "unnamed");
    assert_eq!(slug_of("..."), "unnamed");
    assert_eq!(slug_of(""), "unnamed");
}
