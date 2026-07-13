use super::*;

/// Loads the workspace manifest (`churl.toml`) from the workspace root directory.
pub fn load_workspace_manifest(root: &Path) -> Result<Workspace, PersistenceError> {
    load_value(&root.join(MANIFEST_FILENAME))
}

/// Saves the workspace manifest (`churl.toml`) into the workspace root directory
/// under the strict secret policy, preserving comments and formatting of an
/// existing file.
///
/// A newly-authored name-anchored literal secret refuses with
/// [`PersistenceError::SecretsRefused`]; a pre-existing one (already in the
/// on-disk `churl.toml` baseline) is grandfathered and the save proceeds. Any
/// warnings are discarded — callers surfacing them use
/// [`save_workspace_manifest_checked`].
pub fn save_workspace_manifest(root: &Path, ws: &Workspace) -> Result<(), PersistenceError> {
    save_workspace_manifest_checked(root, ws, SecretPolicy::Strict).map(|_| ())
}

/// Saves the workspace manifest under `policy`, returning the secret
/// [`SecretDecision`] (its warnings) on success. The on-disk `churl.toml` at
/// `root` (when it parses) is the baseline for novelty: an already-present
/// violating var is grandfathered; a new one under [`SecretPolicy::Strict`] with
/// a name anchor refuses with [`PersistenceError::SecretsRefused`].
pub fn save_workspace_manifest_checked(
    root: &Path,
    ws: &Workspace,
    policy: SecretPolicy,
) -> Result<SecretDecision, PersistenceError> {
    let path = root.join(MANIFEST_FILENAME);
    let baseline = load_workspace_manifest(root)
        .map(|old| scan_workspace(&old))
        .unwrap_or_default();
    let decision = crate::secrets::decide(&scan_workspace(ws), &baseline, policy);
    if decision.is_refused() {
        return Err(PersistenceError::SecretsRefused {
            locations: decision.refusal_locations(),
        });
    }
    save_value(&path, ws)?;
    Ok(decision)
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

/// Saves a collection's [`CollectionMeta`] to its `folder.toml` under the strict
/// secret policy, preserving comments and formatting of an existing file (same
/// merge machinery as [`save_workspace_manifest`]).
///
/// A newly-authored name-anchored literal secret refuses with
/// [`PersistenceError::SecretsRefused`]; a pre-existing one (already in the
/// on-disk `folder.toml` baseline) is grandfathered. Warnings are discarded —
/// callers surfacing them use [`save_collection_meta_checked`].
pub fn save_collection_meta(dir: &Path, meta: &CollectionMeta) -> Result<(), PersistenceError> {
    save_collection_meta_checked(dir, meta, SecretPolicy::Strict).map(|_| ())
}

/// Saves a collection's `folder.toml` under `policy`, returning the secret
/// [`SecretDecision`] (its warnings) on success. The on-disk `folder.toml` in
/// `dir` (when present) is the novelty baseline; a new name-anchored literal
/// under [`SecretPolicy::Strict`] refuses with
/// [`PersistenceError::SecretsRefused`].
pub fn save_collection_meta_checked(
    dir: &Path,
    meta: &CollectionMeta,
    policy: SecretPolicy,
) -> Result<SecretDecision, PersistenceError> {
    let baseline = load_collection_meta(dir)
        .map(|old| scan_collection(&old))
        .unwrap_or_default();
    let decision = crate::secrets::decide(&scan_collection(meta), &baseline, policy);
    if decision.is_refused() {
        return Err(PersistenceError::SecretsRefused {
            locations: decision.refusal_locations(),
        });
    }
    save_value(&dir.join(FOLDER_FILENAME), meta)?;
    Ok(decision)
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

    /// The workspace root viewed **as the root collection**. Its `path` is the
    /// workspace root and its `name` is the manifest name (the root collection's
    /// display name). Root-level endpoints are the endpoint files directly under
    /// the root (`churl.toml`/`folder.toml`/`sequences` excluded by
    /// [`Collection::endpoints`]); its sub-collections are [`Self::collections`]
    /// (the `sequences/` skip lives there, root-only).
    pub fn root_collection(&self) -> Collection {
        Collection {
            name: self.manifest.name.clone(),
            path: self.root.clone(),
        }
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

    /// Lists this collection's immediate sub-collections: every child directory
    /// whose name does not start with `.`, sorted by name. Nothing is parsed.
    ///
    /// Unlike [`OpenWorkspace::collections`] (the root), the reserved `sequences/`
    /// directory is **not** skipped here — `sequences` is reserved at the root
    /// level only (it is a global, root-owned role, like profiles). A `sequences`
    /// directory nested under a sub-collection is therefore an ordinary
    /// sub-collection, not churl's sequence store.
    pub fn sub_collections(&self) -> Result<Vec<Collection>, PersistenceError> {
        let read_err = |source| PersistenceError::Read {
            path: self.path.clone(),
            source,
        };
        let mut collections = Vec::new();
        for entry in std::fs::read_dir(&self.path).map_err(read_err)? {
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
