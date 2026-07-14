//! Group-internal reorder: swap a tree node with its sibling neighbour by
//! rewriting `seq`. Mirrors the sequence-editor's step swap-then-renumber, but
//! over on-disk endpoints / collections / sequence files.
//!
//! Reorder is **parent-preserving** — endpoints move only among sibling
//! endpoints, collections only among sibling collections, sequences only among
//! sequences; it never re-parents (that is exclusively move-to). The group is
//! first **densely renumbered `0..N`** in its current sorted order (so
//! hand-written / legacy files that all default to `seq = 0` can be
//! disambiguated) and only then are the two neighbours swapped; a file whose seq
//! is unchanged is left untouched so its comments/formatting survive.

use super::*;

/// Direction of a group-internal reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderDir {
    /// Move the target one slot earlier (toward the top of its group).
    Up,
    /// Move the target one slot later (toward the bottom of its group).
    Down,
}

/// The outcome of a reorder attempt (drives the caller's status line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderOutcome {
    /// The target swapped with its neighbour; affected files were rewritten.
    Moved,
    /// The target is already the first in its group — nothing moved.
    AlreadyFirst,
    /// The target is already the last in its group — nothing moved.
    AlreadyLast,
}

/// The dense `0..N` seq assignment for a group after swapping position `idx` with
/// its neighbour in `dir`. `None` when the target is already at the group edge.
/// Element `i` of the returned vec is the new seq of the item at sorted position
/// `i`.
fn plan_dense_swap(n: usize, idx: usize, dir: ReorderDir) -> Option<Vec<u32>> {
    let swap_with = match dir {
        ReorderDir::Up if idx > 0 => idx - 1,
        ReorderDir::Down if idx + 1 < n => idx + 1,
        _ => return None,
    };
    let mut seqs: Vec<u32> = (0..n as u32).collect();
    seqs.swap(idx, swap_with);
    Some(seqs)
}

/// Shared driver: builds the `(path, seq)` group, sorts it in the same
/// `(seq, filename)` order the loader uses, plans the dense swap for `target`,
/// and persists every item whose seq changed via `save_seq`.
fn reorder_group<F, S>(
    items: Vec<PathBuf>,
    target: &Path,
    dir: ReorderDir,
    load_seq: F,
    mut save_seq: S,
) -> Result<ReorderOutcome, PersistenceError>
where
    F: Fn(&Path) -> Result<u32, PersistenceError>,
    S: FnMut(&Path, u32) -> Result<(), PersistenceError>,
{
    let mut group: Vec<(PathBuf, u32)> = Vec::with_capacity(items.len());
    for path in items {
        let seq = load_seq(&path)?;
        group.push((path, seq));
    }
    group
        .sort_by(|(pa, sa), (pb, sb)| sa.cmp(sb).then_with(|| pa.file_name().cmp(&pb.file_name())));
    let idx = group.iter().position(|(p, _)| p == target).ok_or_else(|| {
        PersistenceError::InvalidDestination {
            reason: "reorder target is not among its siblings".to_owned(),
        }
    })?;
    let Some(new_seqs) = plan_dense_swap(group.len(), idx, dir) else {
        return Ok(match dir {
            ReorderDir::Up => ReorderOutcome::AlreadyFirst,
            ReorderDir::Down => ReorderOutcome::AlreadyLast,
        });
    };
    for (i, (path, cur)) in group.iter().enumerate() {
        if new_seqs[i] != *cur {
            save_seq(path, new_seqs[i])?;
        }
    }
    Ok(ReorderOutcome::Moved)
}

/// The candidate endpoint files in `dir` (every `*.toml` that is not a reserved
/// manifest). Mirrors the loader's `endpoint_files`.
fn endpoint_paths(dir: &Path) -> Result<Vec<PathBuf>, PersistenceError> {
    let read_err = |source| PersistenceError::Read {
        path: dir.to_owned(),
        source,
    };
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(read_err)? {
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

/// The `*.toml` sequence files directly in `dir` (the `sequences/` directory).
fn sequence_paths(dir: &Path) -> Result<Vec<PathBuf>, PersistenceError> {
    let read_err = |source| PersistenceError::Read {
        path: dir.to_owned(),
        source,
    };
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(read_err)? {
        let entry = entry.map_err(read_err)?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        files.push(path);
    }
    Ok(files)
}

/// The sibling collection directories under `parent` (dotfile dirs excluded; the
/// reserved `sequences/` excluded only at the workspace root).
fn sibling_collection_dirs(
    parent: &Path,
    workspace_root: &Path,
) -> Result<Vec<PathBuf>, PersistenceError> {
    let read_err = |source| PersistenceError::Read {
        path: parent.to_owned(),
        source,
    };
    let at_root = parent == workspace_root;
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(read_err)? {
        let entry = entry.map_err(read_err)?;
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
        dirs.push(path);
    }
    Ok(dirs)
}

/// Reorders the endpoint `file` among its sibling endpoints in collection `dir`.
pub fn reorder_endpoint(
    dir: &Path,
    file: &Path,
    direction: ReorderDir,
) -> Result<ReorderOutcome, PersistenceError> {
    reorder_group(
        endpoint_paths(dir)?,
        file,
        direction,
        |p| Ok(load_endpoint(p)?.seq),
        |p, seq| {
            let mut ep = load_endpoint(p)?;
            ep.seq = seq;
            save_endpoint(p, &ep)
        },
    )
}

/// Reorders the sequence `file` among its siblings in the `sequences/` directory
/// `dir`.
pub fn reorder_sequence(
    dir: &Path,
    file: &Path,
    direction: ReorderDir,
) -> Result<ReorderOutcome, PersistenceError> {
    reorder_group(
        sequence_paths(dir)?,
        file,
        direction,
        |p| Ok(load_sequence(p)?.seq),
        |p, seq| {
            let mut s = load_sequence(p)?;
            s.seq = seq;
            save_sequence(p, &s)
        },
    )
}

/// Reorders the collection `dir` among its sibling collections. `workspace_root`
/// scopes the reserved `sequences/` skip (root only).
pub fn reorder_collection(
    dir: &Path,
    workspace_root: &Path,
    direction: ReorderDir,
) -> Result<ReorderOutcome, PersistenceError> {
    let parent = dir.parent().unwrap_or(Path::new("."));
    reorder_group(
        sibling_collection_dirs(parent, workspace_root)?,
        dir,
        direction,
        |p| Ok(load_collection_meta(p)?.seq),
        |p, seq| {
            let mut meta = load_collection_meta(p)?;
            meta.seq = seq;
            save_collection_meta(p, &meta)
        },
    )
}
