//! TOML persistence via `toml_edit`, format-preserving on write.
//!
//! Users hand-edit endpoint files, so comments and key ordering must survive a churl
//! round-trip. Reads deserialize with `toml_edit::de`; writes serialize the value to a
//! fresh document and then *merge* it into the existing file's document, touching only
//! what actually changed so all decor (comments, whitespace, ordering) is preserved.
//!
//! Collections are loaded lazily: opening a workspace parses only `churl.toml`;
//! listing collections stats directories; endpoint files are parsed only when a
//! collection's [`Collection::endpoints`] is called.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::{auth_secret_violations, secret_violations};
use crate::model::{Endpoint, Workspace};

/// Filename of the workspace manifest inside a workspace root.
pub const MANIFEST_FILENAME: &str = "churl.toml";

/// Filename reserved for optional per-collection metadata; never parsed as an endpoint.
pub const FOLDER_FILENAME: &str = "folder.toml";

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
    /// Refused to serialize an endpoint whose auth carries literal secret values
    /// instead of `{{var}}` placeholders (see
    /// [`crate::config::auth_secret_violations`]). Applies to file saves *and*
    /// the stdout TOML path — a redirected stdout is a workspace file too.
    #[error("refusing to save endpoint with literal secret auth values: {}", names.join(", "))]
    SecretsInAuth {
        /// Offending fields as `"auth.<field>"` strings.
        names: Vec<String>,
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
            if name.starts_with('.') || !path.is_dir() {
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

impl Collection {
    /// Parses every `*.toml` file in the collection directory (excluding
    /// `folder.toml`) and returns the endpoints with their file paths, sorted by
    /// `seq` then filename.
    ///
    /// A malformed endpoint file fails the whole call with an error carrying that
    /// file's path — malformed files are never silently skipped.
    pub fn endpoints(&self) -> Result<Vec<(PathBuf, Endpoint)>, PersistenceError> {
        let read_err = |source| PersistenceError::Read {
            path: self.path.clone(),
            source,
        };
        let mut endpoints = Vec::new();
        for entry in std::fs::read_dir(&self.path).map_err(read_err)? {
            let entry = entry.map_err(read_err)?;
            let path = entry.path();
            if !path.is_file()
                || path.extension().and_then(|e| e.to_str()) != Some("toml")
                || path.file_name().and_then(|n| n.to_str()) == Some(FOLDER_FILENAME)
            {
                continue;
            }
            let endpoint = load_endpoint(&path)?;
            endpoints.push((path, endpoint));
        }
        endpoints.sort_by(|(path_a, ep_a), (path_b, ep_b)| {
            ep_a.seq
                .cmp(&ep_b.seq)
                .then_with(|| path_a.file_name().cmp(&path_b.file_name()))
        });
        Ok(endpoints)
    }
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

    std::fs::write(path, output).map_err(|source| PersistenceError::Write {
        path: path.to_owned(),
        source,
    })
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

/// Merges `new` into `old` in place, preserving `old`'s decor wherever the value is
/// unchanged:
///
/// - keys present in `old` but absent in `new` are removed;
/// - unchanged values are left untouched (their comments/formatting survive);
/// - changed scalar values are replaced, copying the old value's decor so inline
///   comments survive;
/// - nested tables recurse; arrays-of-tables of equal length merge element-wise,
///   otherwise the whole array is replaced.
fn merge_tables(old: &mut Table, new: &Table) {
    let stale: Vec<String> = old
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| !new.contains_key(key))
        .collect();
    for key in stale {
        old.remove(&key);
    }
    for (key, new_item) in new.iter() {
        match old.get_mut(key) {
            Some(old_item) => merge_items(old_item, new_item),
            None => {
                old.insert(key, new_item.clone());
            }
        }
    }
}

/// Recursive worker for [`merge_tables`].
fn merge_items(old: &mut Item, new: &Item) {
    match (old, new) {
        (Item::Table(old_table), Item::Table(new_table)) => merge_tables(old_table, new_table),
        (Item::ArrayOfTables(old_tables), Item::ArrayOfTables(new_tables))
            if old_tables.len() == new_tables.len() =>
        {
            for (old_table, new_table) in old_tables.iter_mut().zip(new_tables.iter()) {
                merge_tables(old_table, new_table);
            }
        }
        (Item::Value(old_value), Item::Value(new_value)) => {
            if !values_equal(old_value, new_value) {
                let decor = old_value.decor().clone();
                *old_value = new_value.clone();
                *old_value.decor_mut() = decor;
            }
        }
        (old, new) => *old = new.clone(),
    }
}

/// Semantic value equality, ignoring decor (whitespace/comments) and string style.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(item_a, item_b)| values_equal(item_a, item_b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value_a)| {
                    b.get(key)
                        .is_some_and(|value_b| values_equal(value_a, value_b))
                })
        }
        _ => false,
    }
}
