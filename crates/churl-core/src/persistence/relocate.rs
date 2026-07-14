//! Relocation CRUD: move / copy / duplicate for endpoints and collection
//! subtrees. Mirrors the create/rename atomic-claim discipline
//! ([`claim_endpoint_path`] / [`claim_unused_collection_dir`] + [`ClaimGuard`]):
//! a relocation claims its destination name before writing, suffixing `-N` on a
//! collision, and never clobbers an existing sibling.
//!
//! Ordering: a relocated **endpoint** appends at its destination (`seq =
//! next_seq(dest)`); a relocated **collection subtree** appends among its new
//! siblings (its top `folder.toml` gets `next_collection_seq(dest)`), while its
//! inner contents keep their intra-subtree order verbatim. Reference integrity
//! (rewriting sequence steps that point at a moved path) is the caller's job via
//! [`super::retarget_sequence_steps`] — a *move* rewrites, a *copy/duplicate*
//! never does (the original stays valid; the copy is new and unreferenced).

use super::*;

/// The next collection `seq` among `parent`'s sibling collections: one past their
/// maximum, or `0` when there are none. Mirrors [`next_seq`] for endpoints. The
/// reserved `sequences/` directory (root only) and dotfile dirs are not
/// collections and never counted.
fn next_collection_seq(parent: &Path, workspace_root: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let at_root = parent == workspace_root;
    let mut max: Option<u32> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_dir() || name.starts_with('.') {
            continue;
        }
        if at_root && name == SEQUENCES_DIRNAME {
            continue;
        }
        let seq = load_collection_meta(&path).map(|m| m.seq).unwrap_or(0);
        max = Some(max.map_or(seq, |m| m.max(seq)));
    }
    max.map_or(0, |m| m + 1)
}

/// Copies the endpoint at `src` into `dest_dir` under a fresh (collision-suffixed)
/// slug, appending it at the destination (`seq = next_seq(dest_dir)`). The name,
/// request, and auth are copied verbatim; no sequence step is rewritten (the
/// original stays referenced, the copy is new). Returns the created file's path.
pub fn copy_endpoint(src: &Path, dest_dir: &Path) -> Result<PathBuf, PersistenceError> {
    let mut endpoint = load_endpoint(src)?;
    let slug = src
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| slugify(&endpoint.name));
    let claim = claim_endpoint_path(dest_dir, &slug).map_err(|source| PersistenceError::Write {
        path: dest_dir.to_owned(),
        source,
    })?;
    endpoint.seq = next_seq(dest_dir);
    save_endpoint(claim.path(), &endpoint)?;
    Ok(claim.commit())
}

/// Moves the endpoint at `src` into `dest_dir` (copy into place, then remove the
/// source). A move into the endpoint's current collection is a no-op (returns
/// `src`). Returns the new file path. The caller rewrites referencing sequence
/// steps via [`super::retarget_sequence_steps`].
pub fn move_endpoint(src: &Path, dest_dir: &Path) -> Result<PathBuf, PersistenceError> {
    if src.parent() == Some(dest_dir) {
        return Ok(src.to_owned());
    }
    let new_path = copy_endpoint(src, dest_dir)?;
    delete_endpoint(src)?;
    Ok(new_path)
}

/// Duplicates the endpoint at `src` in place: same collection, a fresh
/// collision-suffixed `<slug>-N.toml`, `seq` appended past the current maximum.
/// Returns the created file's path.
pub fn duplicate_endpoint(src: &Path) -> Result<PathBuf, PersistenceError> {
    let dir = src.parent().unwrap_or(Path::new("."));
    copy_endpoint(src, dir)
}

/// Copies the collection subtree at `src` under `dest_parent`, claiming a fresh
/// (collision-suffixed) directory and recursively copying every file and
/// sub-directory. The copied top collection is re-keyed to append among its new
/// siblings (`next_collection_seq(dest_parent)`); inner contents keep their
/// order. `workspace_root` scopes the reserved `sequences/` skip when appending.
/// Returns the created directory path.
///
/// Mirrors the endpoint path's [`ClaimGuard`] cleanup: the claim creates the dest
/// directory up front, so a failure while populating it removes the partial dir
/// rather than leaving an incomplete collection for the lenient loader to render.
pub fn copy_collection(
    src: &Path,
    dest_parent: &Path,
    workspace_root: &Path,
) -> Result<PathBuf, PersistenceError> {
    reject_into_self(src, dest_parent)?;
    let slug = src
        .file_name()
        .and_then(|n| n.to_str())
        .map(slugify)
        .unwrap_or_else(|| "collection".to_owned());
    let new_dir = claim_unused_collection_dir(dest_parent, &slug).map_err(|source| {
        PersistenceError::Write {
            path: dest_parent.to_owned(),
            source,
        }
    })?;
    // `next_collection_seq` counts the just-created dir, so copying into an all-`0`
    // (sparse) parent yields `seq = 1` — non-minimal but still correctly last.
    let populate = (|| -> Result<(), PersistenceError> {
        copy_dir_recursive(src, &new_dir)?;
        let seq = next_collection_seq(dest_parent, workspace_root);
        if seq != 0 {
            set_collection_seq(&new_dir, seq)?;
        }
        Ok(())
    })();
    if let Err(err) = populate {
        // Best-effort cleanup; the source is untouched (a move deletes it only
        // after a successful copy), so a failed copy leaves the tree clean.
        let _ = std::fs::remove_dir_all(&new_dir);
        return Err(err);
    }
    Ok(new_dir)
}

/// Moves the collection subtree at `src` under `dest_parent` (recursive copy into
/// place, then remove the source). A move into the collection's current parent is
/// a no-op (returns `src`). Rejects a move into `src` itself or a descendant.
/// Returns the new directory path. The caller rewrites referencing sequence steps
/// (whose paths lie under the old prefix) via [`super::retarget_sequence_steps`].
pub fn move_collection(
    src: &Path,
    dest_parent: &Path,
    workspace_root: &Path,
) -> Result<PathBuf, PersistenceError> {
    reject_into_self(src, dest_parent)?;
    if src.parent() == Some(dest_parent) {
        return Ok(src.to_owned());
    }
    let new_dir = copy_collection(src, dest_parent, workspace_root)?;
    delete_collection(src)?;
    Ok(new_dir)
}

/// Duplicates the collection subtree at `src` in place: same parent, a fresh
/// collision-suffixed `<slug>-N` directory, recursive copy. Returns the created
/// directory path.
pub fn duplicate_collection(
    src: &Path,
    workspace_root: &Path,
) -> Result<PathBuf, PersistenceError> {
    let parent = src.parent().unwrap_or(Path::new("."));
    copy_collection(src, parent, workspace_root)
}

/// Rejects placing a collection into itself or one of its own descendants (which
/// would loop the filesystem). Path-component containment, so a sibling whose
/// name merely shares a prefix (`auth` vs `authz`) is not falsely rejected.
fn reject_into_self(src: &Path, dest_parent: &Path) -> Result<(), PersistenceError> {
    if dest_parent.starts_with(src) {
        return Err(PersistenceError::InvalidDestination {
            reason: format!(
                "cannot place {} inside itself or one of its descendants",
                src.display()
            ),
        });
    }
    Ok(())
}

/// Recursively copies the *contents* of `src` into the already-created `dst`
/// directory. Dotfile entries are skipped (they are not part of the churl model,
/// matching the loader's `.`-prefixed exclusion). A per-file/dir IO error is a
/// hard failure.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), PersistenceError> {
    let read_err = |source| PersistenceError::Read {
        path: src.to_owned(),
        source,
    };
    for entry in std::fs::read_dir(src).map_err(read_err)? {
        let entry = entry.map_err(read_err)?;
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        if name.to_str().is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        let target = dst.join(name);
        if path.is_dir() {
            std::fs::create_dir(&target).map_err(|source| PersistenceError::Write {
                path: target.clone(),
                source,
            })?;
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|source| PersistenceError::Write {
                path: target.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Sets a collection's ordering `seq` in its `folder.toml` (load-modify-save,
/// format-preserving). Called only with a non-zero `seq` so a var-less collection
/// never materializes an empty `folder.toml`.
fn set_collection_seq(dir: &Path, seq: u32) -> Result<(), PersistenceError> {
    let mut meta = load_collection_meta(dir)?;
    meta.seq = seq;
    save_collection_meta(dir, &meta)
}
