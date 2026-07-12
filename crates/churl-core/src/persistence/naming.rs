//! Slug + reserved-name derivation extracted from `persistence.rs`: the
//! `slugify` normaliser, the reserved file-stem / directory-name tables, and the
//! collision-avoiding path pickers. Child module of `persistence`, so it keeps
//! full access to the parent's imports/constants (e.g. `SEQUENCES_DIRNAME`)
//! without any visibility widening — pure movement, no logic changes. Items called
//! from `mod.rs` (the CRUD functions) or the parent's inline tests carry
//! `pub(super)`; `slug_of` stays `pub` (public API) and is re-exported from `mod.rs`.

use super::*;

/// File stems churl reserves inside a collection directory. An endpoint (or
/// sequence) file written verbatim as one of these would overshadow a reserved
/// `*.toml` — silently and invisibly: a `churl.toml` endpoint parses as the
/// workspace manifest (the note vanishes), a `folder.toml` endpoint parses as
/// collection metadata. These are the manifest/folder filenames *without* their
/// `.toml` extension.
///
/// `slugify` lowercases, so these match case-insensitively for free — `"Churl"`
/// and `"CHURL.TOML"` slug to `churl` / `churl-toml` respectively; only the bare
/// word is reserved (`churl-toml` is safe). The sequences *directory* is NOT
/// here: a file named `sequences.toml` inside a collection is harmless (the
/// reserved `sequences/` is a top-level directory — see [`RESERVED_DIR_NAMES`]).
///
/// `churl-version` is reserved because it is the slug of the `.churl-version`
/// pin file ([`crate::pin::PIN_FILENAME`]): keeping it off the create/rename
/// path stops an endpoint from being slugged into a name that reads as the
/// version-pin marker.
pub(super) const RESERVED_FILE_STEMS: &[&str] = &[
    // Kept in sync with the filename/pin constants via `reserved_stems_match_constants`.
    "churl",
    "folder",
    "churl-version",
];

/// Directory names churl reserves at the workspace root. A collection directory
/// written verbatim as one of these would shadow churl's own directory — the
/// `sequences/` dir is excluded from [`OpenWorkspace::collections`], so a
/// collection literally named `sequences` would silently vanish from the tree.
/// (The manifest/folder stems are *files*, not directories, so a collection
/// named `churl`/`folder` is not a hazard — only `sequences` is.)
pub(super) const RESERVED_DIR_NAMES: &[&str] = &[SEQUENCES_DIRNAME];

/// Whether `slug` collides with a reserved endpoint/sequence *file* stem.
/// `slugify` already lowercases, so a plain equality check is case-insensitive.
pub(super) fn is_reserved_file_slug(slug: &str) -> bool {
    RESERVED_FILE_STEMS.contains(&slug)
}

/// Whether `slug` collides with a reserved collection *directory* name.
pub(super) fn is_reserved_dir_slug(slug: &str) -> bool {
    RESERVED_DIR_NAMES.contains(&slug)
}

/// The filesystem slug churl would derive from a display `name` (kebab-case,
/// lowercase, ASCII). Public so UI layers can detect when a create/rename was
/// disambiguated — compare this to the final on-disk stem and surface the
/// difference (fail-loud on the reserved-name collision, never a silent bump).
pub fn slug_of(name: &str) -> String {
    slugify(name)
}

/// The filesystem slug churl derives from a display endpoint / sequence /
/// collection name. Runs of non-alphanumeric characters collapse to a single `-`;
/// leading/trailing `-` are trimmed. An empty result falls back to `"unnamed"`.
pub(super) fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Picks an unused `<slug>.toml` path inside `dir`, appending `-2`, `-3`, … on
/// collision (matching the corpus convention of plain suffixes).
///
/// A slug equal to a reserved file stem (see [`RESERVED_FILE_STEMS`]) is treated
/// as already occupied so it disambiguates exactly like a filename clash (`churl`
/// → `churl-2.toml`) instead of clobbering the manifest/folder file — the display
/// `name` inside the file is untouched. Callers surface the final stem when it
/// was bumped (fail-loud, no silent no-op).
pub(super) fn unique_endpoint_path(dir: &Path, slug: &str) -> PathBuf {
    let first = dir.join(format!("{slug}.toml"));
    if !first.exists() && !is_reserved_file_slug(slug) {
        return first;
    }
    let mut n = 2;
    loop {
        let candidate = dir.join(format!("{slug}-{n}.toml"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Resolves the on-disk directory name for a collection `slug` under `parent`,
/// bumping a **reserved** directory slug (see [`RESERVED_DIR_NAMES`] — i.e. a
/// collection literally named `sequences`) to the first free `<slug>-N` so it can
/// never overshadow the sequences directory. A non-reserved slug is returned
/// verbatim — the caller still decides what to do if that directory already
/// exists ([`create_collection`] errors `AlreadyExists`; import reuses it), so
/// this helper does **not** disambiguate on plain existence.
pub(super) fn collection_dir_name(parent: &Path, slug: &str) -> PathBuf {
    if !is_reserved_dir_slug(slug) {
        return parent.join(slug);
    }
    let mut n = 2;
    loop {
        let candidate = parent.join(format!("{slug}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}
