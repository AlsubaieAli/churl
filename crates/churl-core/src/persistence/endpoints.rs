use super::*;

/// Auth-only secret gate for the baseline-free `endpoint_to_toml` stdout path.
/// A redirected stdout is a workspace file too, but there is no on-disk baseline
/// to grandfather against, so this stays a hard block on literal secret *auth*
/// values (`auth.password` / `auth.token` / secret-named `auth.value`) exactly as
/// before — matching import's placeholder-ization contract on the export side.
fn check_auth_secrets(ep: &Endpoint) -> Result<(), PersistenceError> {
    let names: Vec<String> = scan_endpoint(ep)
        .into_iter()
        .filter(|f| f.location.starts_with("auth."))
        .map(|f| f.location)
        .collect();
    if names.is_empty() {
        Ok(())
    } else {
        Err(PersistenceError::SecretsInAuth { names })
    }
}

/// Loads an [`Endpoint`] from a single endpoint TOML file.
pub fn load_endpoint(path: &Path) -> Result<Endpoint, PersistenceError> {
    load_value(path)
}

/// Saves an [`Endpoint`] to `path` under the strict secret policy, preserving
/// comments and formatting of an existing file (see [module docs](self)).
///
/// A newly-authored name-anchored literal secret refuses the write with
/// [`PersistenceError::SecretsRefused`]; a pre-existing one (already present in
/// the on-disk baseline at `path`) is grandfathered and the save proceeds. Any
/// warnings (grandfathered / value-only findings) are discarded — callers that
/// need to surface them use [`save_endpoint_checked`].
pub fn save_endpoint(path: &Path, ep: &Endpoint) -> Result<(), PersistenceError> {
    save_endpoint_checked(path, ep, SecretPolicy::Strict).map(|_| ())
}

/// Saves an [`Endpoint`] to `path` under `policy`, returning the secret
/// [`SecretDecision`] (its warnings) on success. The on-disk endpoint at `path`
/// (when it parses) is the baseline: a violation whose location was already a
/// violation there is grandfathered; a location clean or absent in the baseline
/// is new. A brand-new/unparseable file has an empty baseline, so every violation
/// is new.
///
/// Refuses with [`PersistenceError::SecretsRefused`] iff the decision has
/// refusals (new + name-anchored under [`SecretPolicy::Strict`]); otherwise
/// writes the file and returns the (possibly non-empty) warnings for the caller
/// to surface as a `!` marker.
pub fn save_endpoint_checked(
    path: &Path,
    ep: &Endpoint,
    policy: SecretPolicy,
) -> Result<SecretDecision, PersistenceError> {
    let baseline = load_endpoint(path)
        .map(|old| scan_endpoint(&old))
        .unwrap_or_default();
    let decision = crate::secrets::decide(&scan_endpoint(ep), &baseline, policy);
    if decision.is_refused() {
        return Err(PersistenceError::SecretsRefused {
            locations: decision.refusal_locations(),
        });
    }
    save_value(path, ep)?;
    Ok(decision)
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
    // Atomically claim a free `<slug>.toml` (a concurrent "new endpoint" save can
    // never land on the same file), then fill the reserved placeholder in place.
    // The guard removes the placeholder if the save below fails, so a failed
    // create never leaves a 0-byte endpoint behind.
    let claim =
        claim_endpoint_path(dir, &slugify(name)).map_err(|source| PersistenceError::Write {
            path: dir.to_owned(),
            source,
        })?;
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
    save_endpoint(claim.path(), &endpoint)?;
    Ok(claim.commit())
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
    // `claim_endpoint_path` so it disambiguates even if the file already sat on
    // a reserved stem.
    if !is_reserved_file_slug(&slug) && dir.join(format!("{slug}.toml")) == path {
        return Ok(path.to_owned());
    }
    // Atomically claim the destination name before moving onto it, so a concurrent
    // save can't slip a file into the same slot between choosing and renaming. The
    // guard removes the placeholder if the rename below fails (the `fs::rename`
    // replaces it on success).
    let claim = claim_endpoint_path(dir, &slug).map_err(|source| PersistenceError::Write {
        path: dir.to_owned(),
        source,
    })?;
    std::fs::rename(path, claim.path()).map_err(|source| PersistenceError::Write {
        path: claim.path().to_owned(),
        source,
    })?;
    Ok(claim.commit())
}

/// Deletes the endpoint file at `path`.
pub fn delete_endpoint(path: &Path) -> Result<(), PersistenceError> {
    std::fs::remove_file(path).map_err(|source| PersistenceError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Creates a new collection directory named `name` (slugified) under `parent`. No
/// `folder.toml` is written — that stays lazy until the collection gains vars
/// (matching [`load_collection_meta`]'s missing-is-empty behaviour). Returns the
/// created directory path.
///
/// `workspace_root` is the root of the open workspace. A slug equal to a reserved
/// directory name (see [`RESERVED_DIR_NAMES`], i.e. `sequences`) is disambiguated
/// with a `-2`/`-3` suffix **only when creating at the root** (`parent ==
/// workspace_root`) — `sequences` is reserved at the root level only (M7.9), so a
/// sub-collection literally named `sequences` is created verbatim (matching the
/// loader, which treats a nested `sequences/` as an ordinary collection).
///
/// An empty name is [`PersistenceError::EmptyName`]; an already-existing target
/// directory is [`PersistenceError::AlreadyExists`] (import reuse relies on this).
pub fn create_collection(
    parent: &Path,
    name: &str,
    workspace_root: &Path,
) -> Result<PathBuf, PersistenceError> {
    if name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let slug = slugify(name);
    let reserve = parent == workspace_root;
    // `create_dir` fails atomically if the target already exists, so the claim is
    // the existence check: a non-reserved dir that is already there surfaces as
    // `AlreadyExists` (import reuse relies on the returned path), while a reserved
    // slug (root only) is bumped onto a freshly-created `<slug>-N` with no
    // probe-then-create gap.
    claim_collection_dir(parent, &slug, reserve).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            PersistenceError::AlreadyExists {
                path: parent.join(&slug),
            }
        } else {
            PersistenceError::Write {
                path: parent.join(&slug),
                source,
            }
        }
    })
}

/// Renames the collection directory `dir` to a fresh slug of `new_name` in the
/// same parent. Returns the new directory path.
///
/// `workspace_root` is the root of the open workspace: the reserved-`sequences`
/// bump applies only when `dir` sits directly under the root (M7.9 root-only
/// reservation), so renaming a sub-collection to `sequences` keeps the verbatim
/// name.
///
/// An empty name is [`PersistenceError::EmptyName`]; an existing target directory
/// is [`PersistenceError::AlreadyExists`].
pub fn rename_collection(
    dir: &Path,
    new_name: &str,
    workspace_root: &Path,
) -> Result<PathBuf, PersistenceError> {
    if new_name.trim().is_empty() {
        return Err(PersistenceError::EmptyName);
    }
    let parent = dir.parent().unwrap_or(Path::new("."));
    let reserve = parent == workspace_root;
    let new_dir = collection_dir_name(parent, &slugify(new_name), reserve);
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
