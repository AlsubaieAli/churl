//! TOML persistence via `toml_edit`, format-preserving on write.
//!
//! Users hand-edit endpoint files, so their comments and key ordering must survive a
//! churl round-trip. Writes serialize to a fresh document and *merge* it into the
//! existing file's parsed document, touching only changed values so all decor
//! (comments, whitespace, ordering) is preserved.
//!
//! Collections load lazily: opening a workspace parses only `churl.toml`; endpoint
//! files are parsed only when [`Collection::endpoints`] is called.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::model::{CollectionMeta, Endpoint, Method, OnError, Request, Sequence, Workspace};
use crate::secrets::{
    SecretDecision, SecretPolicy, scan_collection, scan_endpoint, scan_workspace,
};

mod atomic;
mod endpoints;
mod merge;
mod naming;
mod refs;
mod relocate;
mod reorder;
mod sequences;
mod workspace;

// The merge + naming clusters live in child modules. Their items are
// pulled into this module's namespace so in-crate call sites — and `super::*` in
// the inline `tests` module — resolve them unqualified, exactly as before the
// split. `slug_of` is the public API (`persistence::slug_of`), re-exported `pub`
// so its path is unchanged.
use endpoints::next_seq;
use merge::merge_tables;
pub use naming::slug_of;
use naming::{
    ClaimGuard, claim_collection_dir, claim_endpoint_path, claim_unused_collection_dir,
    collection_dir_name, is_reserved_file_slug, slugify,
};

// The atomic write/serialization internals live in `atomic` (a child module).
// Their cross-module helpers are `pub(super)`, pulled into this module's
// namespace so `super::*` (the inline `tests` module) and the other child
// modules resolve them unqualified, exactly as before the split.
use atomic::{load_value, normalize_table, save_value, sort_endpoints};
// `atomic_write` is ALSO reused outside `persistence` (`crate::config::save_defaults`,
// M8.5's config writer), so it is re-exported `pub(crate)` under its full path
// rather than only pulled into this module's local namespace.
pub(crate) use atomic::atomic_write;

// The endpoint + collection CRUD, sequence CRUD, and workspace/collection types
// live in child modules (endpoints/sequences/workspace). Their public items are
// re-exported `pub` so every `persistence::X` path is unchanged.
pub use endpoints::{
    create_collection, create_endpoint, create_endpoint_with, delete_collection, delete_endpoint,
    endpoint_to_toml, load_endpoint, rename_collection, rename_endpoint, save_endpoint,
    save_endpoint_checked,
};
pub use refs::retarget_sequence_steps;
pub use relocate::{
    copy_collection, copy_endpoint, duplicate_collection, duplicate_endpoint, move_collection,
    move_endpoint,
};
pub use reorder::{
    ReorderDir, ReorderOutcome, reorder_collection, reorder_endpoint, reorder_sequence,
};
pub use sequences::{
    create_sequence, delete_sequence, duplicate_sequence, load_sequence, rename_sequence,
    save_sequence,
};
pub use workspace::{
    Collection, CollectionLoad, OpenWorkspace, SequenceLoad, load_collection_meta,
    load_workspace_manifest, save_collection_meta, save_collection_meta_checked,
    save_workspace_manifest, save_workspace_manifest_checked,
};

/// Filename of the workspace manifest inside a workspace root.
pub const MANIFEST_FILENAME: &str = "churl.toml";

/// Filename reserved for optional per-collection metadata; never parsed as an endpoint.
pub const FOLDER_FILENAME: &str = "folder.toml";

/// Reserved workspace subdirectory holding request sequences (`sequences/<slug>.toml`).
/// It is *not* a collection — [`OpenWorkspace::collections`] excludes it.
pub const SEQUENCES_DIRNAME: &str = "sequences";

/// Error reading or writing workspace TOML files.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// A file or directory could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A file could not be written.
    #[error("failed to write {path}: {source}")]
    Write {
        /// Path that failed to write.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A file exists but is not valid TOML for the expected type.
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// Path of the malformed file.
        path: PathBuf,
        /// Underlying TOML deserialization error.
        source: toml_edit::de::Error,
    },
    /// An existing file could not be parsed as a TOML document during a
    /// format-preserving save.
    #[error("failed to parse existing document {path}: {source}")]
    ParseDocument {
        /// Path of the malformed file.
        path: PathBuf,
        /// Underlying TOML syntax error.
        source: toml_edit::TomlError,
    },
    /// A value could not be serialized to TOML.
    #[error("failed to serialize for {path}: {source}")]
    Serialize {
        /// Destination path of the failed save.
        path: PathBuf,
        /// Underlying TOML serialization error.
        source: toml_edit::ser::Error,
    },
    /// Refused to save a workspace manifest containing literal secret-looking
    /// profile variables (secrets are process-env only, never in synced files).
    #[error("refusing to save manifest with secret-looking profile variables: {}", names.join(", "))]
    SecretsInManifest {
        /// Offending variables as `"<profile>.<var>"` strings.
        names: Vec<String>,
    },
    /// Refused to save a collection's `folder.toml` containing literal
    /// secret-looking variables (secrets are process-env only).
    #[error("refusing to save folder.toml with secret-looking variables: {}", names.join(", "))]
    SecretsInCollection {
        /// Offending variables as `"vars.<var>"` strings.
        names: Vec<String>,
    },
    /// Refused to serialize an endpoint whose auth carries literal secret values
    /// instead of `{{var}}` placeholders (see
    /// [`crate::config::auth_secret_violations`]). Applies to file saves *and*
    /// the stdout TOML path — a redirected stdout is a workspace file too.
    #[error("refusing to save endpoint with literal secret auth values: {}", names.join(", "))]
    SecretsInAuth {
        /// Offending fields as `"auth.<field>"` strings.
        names: Vec<String>,
    },
    /// Refused to save because this write would *newly author* one or more
    /// name-anchored literal secrets under the strict policy (see
    /// [`crate::secrets`]). Carries the offending locations (field paths / var
    /// names). Pre-existing secrets are grandfathered and never land here — they
    /// surface as warnings on the successful save instead. This is the widened,
    /// baseline-aware gate covering auth, headers, URL, params, and env vars; the
    /// legacy [`SecretsInAuth`](Self::SecretsInAuth) variant is retained for the
    /// baseline-free `endpoint_to_toml` stdout path.
    #[error("refusing to save: newly-authored literal secret(s): {}", locations.join(", "))]
    SecretsRefused {
        /// Offending locations as field-path / var-name strings.
        locations: Vec<String>,
    },
    /// A CRUD operation was given an empty (or whitespace-only) name.
    #[error("name must not be empty")]
    EmptyName,
    /// A CRUD create/rename target already exists on disk (refuse to clobber).
    #[error("already exists: {path}")]
    AlreadyExists {
        /// The path that already exists.
        path: PathBuf,
    },
    /// A move/copy destination is invalid — e.g. relocating a collection into
    /// itself or one of its own descendants (which would loop the filesystem).
    #[error("invalid destination: {reason}")]
    InvalidDestination {
        /// Human-readable reason the destination was rejected.
        reason: String,
    },
    /// The manifest carries a proxy URL with embedded credentials
    /// (`user:pass@host`). Credentials in a synced workspace file are a leak, so
    /// the save is refused rather than silently stripping them — set a
    /// credentialed proxy from `--proxy`/env/the Options overlay (session-scoped)
    /// instead.
    #[error("refusing to save churl.toml with a credentialed proxy URL: {proxy}")]
    ProxyCredentialsRefused {
        /// The offending proxy string (as authored; the caller may mask it).
        proxy: String,
    },
}

#[cfg(test)]
mod tests {
    use super::atomic::atomic_write;
    use super::naming::{RESERVED_DIR_NAMES, RESERVED_FILE_STEMS, is_reserved_dir_slug};
    use super::*;

    /// The reserved sets must stay in sync with the filename/dir constants, or an
    /// endpoint could still be written over a renamed manifest/folder file, or a
    /// collection could shadow the sequences dir. Guards against silent drift.
    #[test]
    fn reserved_stems_match_constants() {
        let manifest_stem = MANIFEST_FILENAME.trim_end_matches(".toml");
        let folder_stem = FOLDER_FILENAME.trim_end_matches(".toml");
        // File stems: the manifest + folder metadata filenames without `.toml`.
        assert!(
            RESERVED_FILE_STEMS.contains(&manifest_stem),
            "manifest stem missing"
        );
        assert!(
            RESERVED_FILE_STEMS.contains(&folder_stem),
            "folder stem missing"
        );
        // The `.churl-version` pin file's slug must be reserved too, so no
        // create/rename slugs an endpoint into the version-pin marker name.
        let pin_stem = crate::pin::PIN_FILENAME.trim_start_matches('.');
        assert!(
            RESERVED_FILE_STEMS.contains(&pin_stem),
            "pin stem missing: {pin_stem}"
        );
        // Dir names: the sequences directory.
        assert!(
            RESERVED_DIR_NAMES.contains(&SEQUENCES_DIRNAME),
            "sequences dir missing"
        );
        // Each reserved stem is genuinely reserved case-insensitively (slugify
        // lowercases): the bare word never lands on disk verbatim.
        for stem in RESERVED_FILE_STEMS {
            assert!(is_reserved_file_slug(&slugify(stem)));
            assert!(is_reserved_file_slug(&slugify(&stem.to_uppercase())));
        }
        for name in RESERVED_DIR_NAMES {
            assert!(is_reserved_dir_slug(&slugify(name)));
        }
        // A collection named `folder`/`churl` is NOT a dir hazard (those are files).
        assert!(!is_reserved_dir_slug(&slugify("folder")));
        assert!(!is_reserved_dir_slug(&slugify("churl")));
        // A name whose slug merely *contains* a reserved stem is NOT reserved.
        assert!(!is_reserved_file_slug(&slugify("churl.toml"))); // → "churl-toml"
        assert!(!is_reserved_file_slug(&slugify("my churl"))); // → "my-churl"
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.toml");

        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        // The sibling temp file is renamed away on success — nothing left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    /// A failing write must never replace a good pre-existing file, and must not
    /// leave a torn temp file behind. We inject the failure by making the parent
    /// directory read-only so the temp `File::create` fails before any rename.
    #[cfg(unix)]
    #[test]
    fn torn_write_never_replaces_a_good_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.toml");

        atomic_write(&path, b"good original").unwrap();

        // Make the directory read+execute but not writable: temp creation fails.
        let ro = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(dir.path(), ro).unwrap();

        let result = atomic_write(&path, b"torn replacement");

        // Restore writability so the tempdir can be cleaned up regardless.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "write into a read-only dir should fail");
        // The original file is completely intact — the rename never ran.
        assert_eq!(std::fs::read(&path).unwrap(), b"good original");

        // No torn temp file survives the failure.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    /// The claim guard removes its placeholder on drop when the caller never
    /// commits (a post-claim failure), and leaves it alone once committed (the
    /// success path). Proves the cleanup semantics without injecting an I/O error.
    #[test]
    fn claim_guard_removes_placeholder_unless_committed() {
        let dir = tempfile::tempdir().unwrap();

        // Uncommitted claim: dropping the guard deletes the 0-byte placeholder.
        let guard = claim_endpoint_path(dir.path(), "ping").unwrap();
        let claimed = guard.path().to_owned();
        assert!(claimed.exists(), "claim creates the placeholder");
        drop(guard);
        assert!(
            !claimed.exists(),
            "an uncommitted claim removes the placeholder on drop"
        );

        // Committed claim: the file survives, and `commit` hands back the path.
        let guard = claim_endpoint_path(dir.path(), "ping").unwrap();
        let path = guard.commit();
        assert!(
            path.exists(),
            "a committed claim leaves the file in place: {path:?}"
        );
    }

    /// Deterministic post-claim failure: with the collection dir made read-only
    /// after the claim, the real save fails and the guard's drop removes the
    /// placeholder — no 0-byte endpoint is left behind for the loader to choke on.
    #[cfg(unix)]
    #[test]
    fn claim_placeholder_is_cleaned_up_when_save_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let coll = dir.path().join("c");
        std::fs::create_dir(&coll).unwrap();

        let claimed = {
            let claim = claim_endpoint_path(&coll, "ping").unwrap();
            let claimed = claim.path().to_owned();
            assert!(claimed.exists(), "placeholder is on disk");

            // Read-only dir: `atomic_write`'s temp create fails, so the save errors
            // out *after* the claim — exactly the leak window the guard covers.
            std::fs::set_permissions(&coll, std::fs::Permissions::from_mode(0o500)).unwrap();
            let ep = Endpoint {
                seq: 0,
                name: "Ping".into(),
                assertions: Vec::new(),
                extract: std::collections::BTreeMap::new(),
                persist: Vec::new(),
                request: Request {
                    method: Method::Get,
                    url: String::new(),
                    headers: Vec::new(),
                    params: Vec::new(),
                    body: None,
                    auth: None,
                    insecure: false,
                },
            };
            let result = save_endpoint(claim.path(), &ep);
            // Restore writability so both the guard drop and tempdir cleanup work.
            std::fs::set_permissions(&coll, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.is_err(), "save into a read-only dir must fail");
            claimed
            // `claim` drops here, armed (never committed) → placeholder removed.
        };

        assert!(
            !claimed.exists(),
            "a failed post-claim save leaves no stray placeholder"
        );
    }
}
