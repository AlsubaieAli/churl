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

use crate::config::{auth_secret_violations, collection_secret_violations, secret_violations};
use crate::model::{CollectionMeta, Endpoint, Method, OnError, Request, Sequence, Workspace};

mod merge;
mod naming;

// The merge + naming clusters live in child modules (M7.11). Their items are
// pulled into this module's namespace so in-crate call sites — and `super::*` in
// the inline `tests` module — resolve them unqualified, exactly as before the
// split. `slug_of` is the public API (`persistence::slug_of`), re-exported `pub`
// so its path is unchanged.
use merge::merge_tables;
pub use naming::slug_of;
use naming::{collection_dir_name, is_reserved_file_slug, slugify, unique_endpoint_path};

/// Filename of the workspace manifest inside a workspace root.
pub const MANIFEST_FILENAME: &str = "churl.toml";

/// Filename reserved for optional per-collection metadata; never parsed as an endpoint.
pub const FOLDER_FILENAME: &str = "folder.toml";

/// Reserved workspace subdirectory holding request sequences (`sequences/<slug>.toml`).
/// It is *not* a collection — [`OpenWorkspace::collections`] excludes it.
pub const SEQUENCES_DIRNAME: &str = "sequences";

/// Error reading or writing workspace TOML files.
#[derive(Debug, thiserror::Error)]
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
    /// A CRUD operation was given an empty (or whitespace-only) name.
    #[error("name must not be empty")]
    EmptyName,
    /// A CRUD create/rename target already exists on disk (refuse to clobber).
    #[error("already exists: {path}")]
    AlreadyExists {
        /// The path that already exists.
        path: PathBuf,
    },
}

/// Gate on [`crate::config::auth_secret_violations`] shared by every
/// churl-initiated endpoint serialization.
fn check_auth_secrets(ep: &Endpoint) -> Result<(), PersistenceError> {
    let violations = auth_secret_violations(ep);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(PersistenceError::SecretsInAuth { names: violations })
    }
}

/// Loads an [`Endpoint`] from a single endpoint TOML file.
pub fn load_endpoint(path: &Path) -> Result<Endpoint, PersistenceError> {
    load_value(path)
}

/// Saves an [`Endpoint`] to `path`, preserving comments and formatting of an
/// existing file (see [module docs](self)).
///
/// Refuses with [`PersistenceError::SecretsInAuth`] when the endpoint's auth
/// carries literal secret values instead of `{{var}}` placeholders.
pub fn save_endpoint(path: &Path, ep: &Endpoint) -> Result<(), PersistenceError> {
    check_auth_secrets(ep)?;
    save_value(path, ep)
}

/// Serializes an [`Endpoint`] to its on-disk TOML shape (identical to a fresh
/// [`save_endpoint`]) without touching the filesystem — used by `churl import`
/// to print to stdout.
///
/// Refuses with [`PersistenceError::SecretsInAuth`] exactly like
/// [`save_endpoint`]: a redirected stdout is a workspace file too.
pub fn endpoint_to_toml(ep: &Endpoint) -> Result<String, PersistenceError> {
    check_auth_secrets(ep)?;
    let mut doc =
        toml_edit::ser::to_document(ep).map_err(|source| PersistenceError::Serialize {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
    normalize_table(doc.as_table_mut());
    Ok(doc.to_string())
}

/// The next `seq` for a new endpoint in `dir`: one past the maximum existing
/// endpoint `seq` (plain +1 — the corpus uses no fixed step), or `0` when the
/// collection is empty. Unreadable/malformed files are ignored here (an empty
/// collection and a broken one both start at `0`; broken files surface on load).
fn next_seq(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut max: Option<u32> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path.extension().and_then(|e| e.to_str()) != Some("toml")
            || path.file_name().and_then(|n| n.to_str()) == Some(FOLDER_FILENAME)
        {
            continue;
        }
        if let Ok(ep) = load_endpoint(&path) {
            max = Some(max.map_or(ep.seq, |m| m.max(ep.seq)));
        }
    }
    max.map_or(0, |m| m + 1)
}

/// Creates a new endpoint file in the collection directory `dir` with a default
/// GET request and an empty URL. The filename is a slug of `name`
/// (collision-suffixed); `seq` is auto-assigned to one past the collection's
/// current maximum (see [`next_seq`]). Returns the created file's path.
///
/// An empty name is [`PersistenceError::EmptyName`]. The default endpoint carries
/// no auth, so the secrets gate is never hit here.
pub fn create_endpoint(dir: &Path, name: &str) -> Result<PathBuf, PersistenceError> {
    if name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let path = unique_endpoint_path(dir, &slugify(name));
    let endpoint = Endpoint {
        seq: next_seq(dir),
        name: name.trim().to_owned(),
        request: Request {
            method: Method::Get,
            url: String::new(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
        },
    };
    save_endpoint(&path, &endpoint)?;
    Ok(path)
}

/// Renames the endpoint at `path`: updates its `name` (via the format-preserving
/// merge save) *and* renames the file to a fresh slug of `new_name` in the same
/// directory. Both changes happen or neither — the file is written under the old
/// path first, then moved, so a secrets refusal aborts before any move. Returns
/// the new file path.
///
/// An empty `new_name` is [`PersistenceError::EmptyName`]; a slug collision with a
/// different existing file is [`PersistenceError::AlreadyExists`].
pub fn rename_endpoint(path: &Path, new_name: &str) -> Result<PathBuf, PersistenceError> {
    if new_name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let mut endpoint = load_endpoint(path)?;
    endpoint.name = new_name.trim().to_owned();
    // Write the name change first (this also runs the secrets gate).
    save_endpoint(path, &endpoint)?;

    let dir = path.parent().unwrap_or(Path::new("."));
    let slug = slugify(new_name);
    // An unchanged, non-reserved slug keeps the file where it is — its own path
    // must not count as a collision. A reserved slug always falls through to
    // `unique_endpoint_path` so it disambiguates even if the file already sat on
    // a reserved stem.
    if !is_reserved_file_slug(&slug) && dir.join(format!("{slug}.toml")) == path {
        return Ok(path.to_owned());
    }
    let new_path = unique_endpoint_path(dir, &slug);
    std::fs::rename(path, &new_path).map_err(|source| PersistenceError::Write {
        path: new_path.clone(),
        source,
    })?;
    Ok(new_path)
}

/// Deletes the endpoint file at `path`.
pub fn delete_endpoint(path: &Path) -> Result<(), PersistenceError> {
    std::fs::remove_file(path).map_err(|source| PersistenceError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Creates a new collection directory named `name` (slugified) under `root`. No
/// `folder.toml` is written — that stays lazy until the collection gains vars
/// (matching [`load_collection_meta`]'s missing-is-empty behaviour). Returns the
/// created directory path.
///
/// A slug equal to a reserved directory name (see [`RESERVED_DIR_NAMES`], i.e.
/// `sequences`) is disambiguated with a `-2`/`-3` suffix so it never overshadows
/// churl's own directories — the display name (the on-disk dir name) is what
/// callers surface.
///
/// An empty name is [`PersistenceError::EmptyName`]; an already-existing target
/// directory is [`PersistenceError::AlreadyExists`] (import reuse relies on this).
pub fn create_collection(root: &Path, name: &str) -> Result<PathBuf, PersistenceError> {
    if name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let dir = collection_dir_name(root, &slugify(name));
    if dir.exists() {
        return Err(PersistenceError::AlreadyExists { path: dir });
    }
    std::fs::create_dir(&dir).map_err(|source| PersistenceError::Write {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

/// Renames the collection directory `dir` to a fresh slug of `new_name` in the
/// same parent. Returns the new directory path.
///
/// An empty name is [`PersistenceError::EmptyName`]; an existing target directory
/// is [`PersistenceError::AlreadyExists`].
pub fn rename_collection(dir: &Path, new_name: &str) -> Result<PathBuf, PersistenceError> {
    if new_name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let parent = dir.parent().unwrap_or(Path::new("."));
    let new_dir = collection_dir_name(parent, &slugify(new_name));
    if new_dir == dir {
        return Ok(new_dir);
    }
    if new_dir.exists() {
        return Err(PersistenceError::AlreadyExists { path: new_dir });
    }
    std::fs::rename(dir, &new_dir).map_err(|source| PersistenceError::Write {
        path: new_dir.clone(),
        source,
    })?;
    Ok(new_dir)
}

/// Deletes the collection directory `dir` and everything inside it.
pub fn delete_collection(dir: &Path) -> Result<(), PersistenceError> {
    std::fs::remove_dir_all(dir).map_err(|source| PersistenceError::Write {
        path: dir.to_owned(),
        source,
    })
}

/// Loads a [`Sequence`] from a single `sequences/<slug>.toml` file.
pub fn load_sequence(path: &Path) -> Result<Sequence, PersistenceError> {
    load_value(path)
}

/// Saves a [`Sequence`] to `path`, preserving comments and formatting of an
/// existing file (same merge machinery as endpoints). No secrets gate is needed —
/// a sequence file holds endpoint refs and extraction expressions (var *names* →
/// expressions), never secret values.
pub fn save_sequence(path: &Path, sequence: &Sequence) -> Result<(), PersistenceError> {
    save_value(path, sequence)
}

/// Picks an unused `<slug>.toml` path for a sequence inside `dir` (reuses the
/// endpoint collision-suffix convention).
fn unique_sequence_path(dir: &Path, slug: &str) -> PathBuf {
    unique_endpoint_path(dir, slug)
}

/// The next `seq` for a new sequence in `dir`: one past the maximum existing
/// sequence `seq`, or `0` when empty. Unreadable/malformed files are ignored.
fn next_sequence_seq(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut max: Option<u32> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if let Ok(sequence) = load_sequence(&path) {
            max = Some(max.map_or(sequence.seq, |m| m.max(sequence.seq)));
        }
    }
    max.map_or(0, |m| m + 1)
}

/// Creates a new empty sequence in the workspace's `sequences/` dir (created on
/// demand). The filename is a slug of `name` (collision-suffixed); `seq` is
/// auto-assigned to one past the current maximum; `on_error` defaults to
/// [`OnError::Halt`]. Returns the created file's path.
///
/// An empty name is [`PersistenceError::EmptyName`].
pub fn create_sequence(root: &Path, name: &str) -> Result<PathBuf, PersistenceError> {
    if name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let dir = root.join(SEQUENCES_DIRNAME);
    std::fs::create_dir_all(&dir).map_err(|source| PersistenceError::Write {
        path: dir.clone(),
        source,
    })?;
    let path = unique_sequence_path(&dir, &slugify(name));
    let sequence = Sequence {
        seq: next_sequence_seq(&dir),
        name: name.trim().to_owned(),
        on_error: OnError::Halt,
        steps: Vec::new(),
    };
    save_sequence(&path, &sequence)?;
    Ok(path)
}

/// Renames the sequence at `path`: updates its `name` (via the format-preserving
/// merge save) *and* renames the file to a fresh slug of `new_name` in the same
/// directory. Both changes happen or neither. Returns the new file path.
///
/// An empty `new_name` is [`PersistenceError::EmptyName`].
pub fn rename_sequence(path: &Path, new_name: &str) -> Result<PathBuf, PersistenceError> {
    if new_name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let mut sequence = load_sequence(path)?;
    sequence.name = new_name.trim().to_owned();
    save_sequence(path, &sequence)?;

    let dir = path.parent().unwrap_or(Path::new("."));
    let slug = slugify(new_name);
    if dir.join(format!("{slug}.toml")) == path {
        return Ok(path.to_owned());
    }
    let new_path = unique_sequence_path(dir, &slug);
    std::fs::rename(path, &new_path).map_err(|source| PersistenceError::Write {
        path: new_path.clone(),
        source,
    })?;
    Ok(new_path)
}

/// Deletes the sequence file at `path`.
pub fn delete_sequence(path: &Path) -> Result<(), PersistenceError> {
    std::fs::remove_file(path).map_err(|source| PersistenceError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Loads the workspace manifest (`churl.toml`) from the workspace root directory.
pub fn load_workspace_manifest(root: &Path) -> Result<Workspace, PersistenceError> {
    load_value(&root.join(MANIFEST_FILENAME))
}

/// Saves the workspace manifest (`churl.toml`) into the workspace root directory,
/// preserving comments and formatting of an existing file.
///
/// Refuses with [`PersistenceError::SecretsInManifest`] when any profile variable
/// looks like a literal secret (see [`crate::config::secret_violations`]).
pub fn save_workspace_manifest(root: &Path, ws: &Workspace) -> Result<(), PersistenceError> {
    let violations = secret_violations(ws);
    if !violations.is_empty() {
        return Err(PersistenceError::SecretsInManifest { names: violations });
    }
    save_value(&root.join(MANIFEST_FILENAME), ws)
}

/// Loads a collection's [`CollectionMeta`] from its `folder.toml`. A missing file
/// is not an error and yields [`CollectionMeta::default`] (an empty var table).
pub fn load_collection_meta(dir: &Path) -> Result<CollectionMeta, PersistenceError> {
    let path = dir.join(FOLDER_FILENAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml_edit::de::from_str(&text)
            .map_err(|source| PersistenceError::Parse { path, source }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(CollectionMeta::default()),
        Err(source) => Err(PersistenceError::Read { path, source }),
    }
}

/// Saves a collection's [`CollectionMeta`] to its `folder.toml`, preserving
/// comments and formatting of an existing file (same merge machinery as
/// [`save_workspace_manifest`]).
///
/// Refuses with [`PersistenceError::SecretsInCollection`] when any variable looks
/// like a literal secret (see [`crate::config::collection_secret_violations`]).
pub fn save_collection_meta(dir: &Path, meta: &CollectionMeta) -> Result<(), PersistenceError> {
    let violations = collection_secret_violations(meta);
    if !violations.is_empty() {
        return Err(PersistenceError::SecretsInCollection { names: violations });
    }
    save_value(&dir.join(FOLDER_FILENAME), meta)
}

/// A workspace opened lazily: only `churl.toml` has been parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspace {
    root: PathBuf,
    manifest: Workspace,
}

impl OpenWorkspace {
    /// Opens the workspace at `root`, reading only its `churl.toml` manifest.
    /// No collection directory is scanned and no endpoint file is parsed.
    pub fn open(root: &Path) -> Result<Self, PersistenceError> {
        let manifest = load_workspace_manifest(root)?;
        Ok(Self {
            root: root.to_owned(),
            manifest,
        })
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The parsed workspace manifest.
    pub fn manifest(&self) -> &Workspace {
        &self.manifest
    }

    /// Lists the workspace's collections: every immediate subdirectory of the root
    /// whose name does not start with `.`, sorted by name. Nothing is parsed —
    /// endpoint files are read only when [`Collection::endpoints`] is called.
    pub fn collections(&self) -> Result<Vec<Collection>, PersistenceError> {
        let read_err = |source| PersistenceError::Read {
            path: self.root.clone(),
            source,
        };
        let mut collections = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(read_err)? {
            let entry = entry.map_err(read_err)?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name == SEQUENCES_DIRNAME || !path.is_dir() {
                continue;
            }
            collections.push(Collection {
                name: name.to_owned(),
                path,
            });
        }
        collections.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(collections)
    }

    /// Loads every request sequence from the workspace's `sequences/` directory,
    /// sorted by `seq` then filename. A missing `sequences/` dir is not an error
    /// (yields an empty load). Uses the **lenient** posture: a single unparseable
    /// sequence file degrades to a warning instead of aborting the whole load —
    /// but is never silently swallowed. Only the directory read itself is a hard
    /// error.
    pub fn sequences(&self) -> Result<SequenceLoad, PersistenceError> {
        let dir = self.root.join(SEQUENCES_DIRNAME);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SequenceLoad::default());
            }
            Err(source) => return Err(PersistenceError::Read { path: dir, source }),
        };
        let read_err = |source| PersistenceError::Read {
            path: dir.clone(),
            source,
        };
        let mut sequences = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries {
            let path = entry.map_err(read_err)?.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match load_sequence(&path) {
                Ok(sequence) => sequences.push((path, sequence)),
                Err(err) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<file>");
                    let reason = match &err {
                        PersistenceError::Parse { source, .. } => source.to_string(),
                        other => other.to_string(),
                    };
                    warnings.push(format!("skipped {name}: {reason}"));
                }
            }
        }
        sequences.sort_by(|(path_a, seq_a), (path_b, seq_b)| {
            seq_a
                .seq
                .cmp(&seq_b.seq)
                .then_with(|| path_a.file_name().cmp(&path_b.file_name()))
        });
        Ok(SequenceLoad {
            sequences,
            warnings,
        })
    }
}

/// The outcome of a lenient sequence load ([`OpenWorkspace::sequences`]): the
/// successfully-parsed sequences plus one warning per unparseable file. Mirrors
/// [`CollectionLoad`] — a single bad file never aborts the load, never silently
/// vanishes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceLoad {
    /// Successfully parsed sequences with their file paths, sorted by `seq` then
    /// filename.
    pub sequences: Vec<(PathBuf, Sequence)>,
    /// Human-readable warnings, one per skipped/unparseable file.
    pub warnings: Vec<String>,
}

/// A collection: a directory of endpoint files inside a workspace. Holds only the
/// name and path until [`Collection::endpoints`] parses its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    /// Directory name, used as the collection's display name.
    pub name: String,
    /// Absolute or workspace-relative path of the collection directory.
    pub path: PathBuf,
}

/// The outcome of a lenient collection load ([`Collection::endpoints_lenient`]):
/// the successfully-parsed endpoints plus one warning per file that could not be
/// parsed. A single unparseable endpoint never aborts the load — but it is never
/// swallowed silently either (Constitution: fail loudly).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectionLoad {
    /// Successfully parsed endpoints with their file paths, sorted by `seq` then
    /// filename.
    pub endpoints: Vec<(PathBuf, Endpoint)>,
    /// Human-readable warnings, one per skipped/unparseable file, e.g.
    /// `"skipped user.toml: missing field `request`"`.
    pub warnings: Vec<String>,
}

impl Collection {
    /// Lists the candidate endpoint files in the collection directory: every
    /// `*.toml` that is not a reserved manifest (`folder.toml` or `churl.toml`).
    ///
    /// A `churl.toml` is skipped because a nested-workspace layout (a collection
    /// dir that is itself a workspace root) would otherwise have its manifest
    /// parsed as an [`Endpoint`] (`missing field 'request'`) and abort the whole
    /// load. Directory (`read_dir`) IO errors are hard errors — a real failure,
    /// not a single bad file.
    fn endpoint_files(&self) -> Result<Vec<PathBuf>, PersistenceError> {
        let read_err = |source| PersistenceError::Read {
            path: self.path.clone(),
            source,
        };
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.path).map_err(read_err)? {
            let entry = entry.map_err(read_err)?;
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str());
            if !path.is_file()
                || path.extension().and_then(|e| e.to_str()) != Some("toml")
                || name == Some(FOLDER_FILENAME)
                || name == Some(MANIFEST_FILENAME)
            {
                continue;
            }
            files.push(path);
        }
        Ok(files)
    }

    /// Parses every `*.toml` file in the collection directory (excluding the
    /// reserved `folder.toml` and `churl.toml`) and returns the endpoints with
    /// their file paths, sorted by `seq` then filename.
    ///
    /// A malformed endpoint file fails the whole call with an error carrying that
    /// file's path — malformed files are never silently skipped. Callers that
    /// must survive one bad file (the TUI load path) use
    /// [`Collection::endpoints_lenient`] instead.
    pub fn endpoints(&self) -> Result<Vec<(PathBuf, Endpoint)>, PersistenceError> {
        let mut endpoints = Vec::new();
        for path in self.endpoint_files()? {
            let endpoint = load_endpoint(&path)?;
            endpoints.push((path, endpoint));
        }
        sort_endpoints(&mut endpoints);
        Ok(endpoints)
    }

    /// Like [`Collection::endpoints`], but degrades a per-file parse failure to a
    /// warning instead of aborting: the returned [`CollectionLoad`] carries the
    /// successfully-parsed endpoints plus one warning naming each unparseable
    /// file. Only the directory read itself is a hard error.
    ///
    /// This is the load path the TUI uses so a single hand-corrupted endpoint (or
    /// a stray non-endpoint `.toml`) can never nuke the whole workspace load.
    pub fn endpoints_lenient(&self) -> Result<CollectionLoad, PersistenceError> {
        let mut endpoints = Vec::new();
        let mut warnings = Vec::new();
        for path in self.endpoint_files()? {
            match load_endpoint(&path) {
                Ok(endpoint) => endpoints.push((path, endpoint)),
                Err(err) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<file>");
                    let reason = match &err {
                        PersistenceError::Parse { source, .. } => source.to_string(),
                        other => other.to_string(),
                    };
                    warnings.push(format!("skipped {name}: {reason}"));
                }
            }
        }
        sort_endpoints(&mut endpoints);
        Ok(CollectionLoad {
            endpoints,
            warnings,
        })
    }
}

/// Sorts endpoints by `seq`, then filename (the stable order used everywhere the
/// explorer and search show endpoints).
fn sort_endpoints(endpoints: &mut [(PathBuf, Endpoint)]) {
    endpoints.sort_by(|(path_a, ep_a), (path_b, ep_b)| {
        ep_a.seq
            .cmp(&ep_b.seq)
            .then_with(|| path_a.file_name().cmp(&path_b.file_name()))
    });
}

/// Loads and deserializes a TOML file into `T`.
fn load_value<T: DeserializeOwned>(path: &Path) -> Result<T, PersistenceError> {
    let text = std::fs::read_to_string(path).map_err(|source| PersistenceError::Read {
        path: path.to_owned(),
        source,
    })?;
    toml_edit::de::from_str(&text).map_err(|source| PersistenceError::Parse {
        path: path.to_owned(),
        source,
    })
}

/// Serializes `value` and writes it to `path` with a format-preserving merge: if the
/// file already exists, only changed values are replaced in its parsed document so
/// comments, ordering, and whitespace survive.
fn save_value<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistenceError> {
    let mut fresh =
        toml_edit::ser::to_document(value).map_err(|source| PersistenceError::Serialize {
            path: path.to_owned(),
            source,
        })?;
    normalize_table(fresh.as_table_mut());

    let output = match std::fs::read_to_string(path) {
        Ok(existing) => {
            let mut doc: DocumentMut =
                existing
                    .parse()
                    .map_err(|source| PersistenceError::ParseDocument {
                        path: path.to_owned(),
                        source,
                    })?;
            merge_tables(doc.as_table_mut(), fresh.as_table());
            doc.to_string()
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fresh.to_string(),
        Err(source) => {
            return Err(PersistenceError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };

    atomic_write(path, output.as_bytes()).map_err(|source| PersistenceError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Durably writes `bytes` to `path`, replacing any existing file atomically.
///
/// A crash mid-write must never leave the source-of-truth file torn: we write to a
/// sibling temp file in the same directory (same filesystem → `rename` is atomic),
/// `fsync` the data, then `rename` over `path`. Finally we `fsync` the parent
/// directory so the rename itself survives power loss. On any error the temp file is
/// removed best-effort and the underlying I/O error is returned.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "churl".to_owned());

    // Unique-ish sibling name so concurrent writers don't collide on one temp path.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = dir.join(format!(".{}.{}.{}.tmp", file_name, std::process::id(), seq));

    // Write + fsync the data into the temp file, then rename it over the target.
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, path)
    })();

    if let Err(err) = result {
        // Best-effort cleanup; the original file is untouched (rename never ran, or
        // the earlier steps failed before it).
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    // fsync the parent directory so the rename is durable across power loss. Opening
    // a directory for fsync is unsupported on some platforms (notably Windows); a
    // failure here must not fail an otherwise-successful save, so we degrade quietly.
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

/// Normalizes a freshly serialized table in place so it renders as idiomatic TOML:
/// inline tables become standard `[table]`s and arrays of inline tables become
/// `[[array-of-tables]]` (e.g. `[[request.headers]]`).
fn normalize_table(table: &mut Table) {
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    for key in keys {
        if let Some(item) = table.get_mut(&key) {
            normalize_item(item);
        }
    }
}

/// Recursive worker for [`normalize_table`].
fn normalize_item(item: &mut Item) {
    match item {
        Item::Value(Value::Array(array))
            if !array.is_empty() && array.iter().all(|v| matches!(v, Value::InlineTable(_))) =>
        {
            let mut tables = ArrayOfTables::new();
            for value in array.iter() {
                if let Value::InlineTable(inline) = value {
                    let mut table = inline.clone().into_table();
                    normalize_table(&mut table);
                    tables.push(table);
                }
            }
            *item = Item::ArrayOfTables(tables);
        }
        Item::Value(Value::InlineTable(inline)) => {
            let mut table = inline.clone().into_table();
            normalize_table(&mut table);
            *item = Item::Table(table);
        }
        Item::Table(table) => normalize_table(table),
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                normalize_table(table);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
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
}
