//! Sequence-step reference integrity: when an endpoint or collection changes its
//! on-disk path (a rename or a move), the sequence steps that point at it must be
//! repointed, or a later run resolves a vanished path. Sequence steps reference
//! endpoints by **workspace-relative** path (`resolve_step_path` does
//! `root.join(rel)`), so the rewrite works purely on those relative strings.
//!
//! This is the single, fail-loud helper for that rewrite. It also closes the
//! pre-existing latent bug where a plain rename silently broke referencing
//! sequences. Copy/duplicate never call it (the original stays valid; the copy is
//! new and unreferenced).

use std::path::Component;

use super::*;

/// Rewrites every sequence step whose `endpoint` path is `old` (a moved/renamed
/// endpoint) or lies under `old` as a directory prefix (a moved/renamed
/// collection), repointing it to `new`. Both are **workspace-relative** paths
/// (`old`/`new` as a step stores them, e.g. `auth/login.toml` for an endpoint or
/// `auth` for a collection dir). Returns the number of steps rewritten across all
/// sequences.
///
/// A missing `sequences/` directory is not an error (returns `0`). An unparseable
/// sequence file is skipped here (it surfaces loudly on the normal load path);
/// only a real IO error on the directory read or a step-rewriting save aborts.
pub fn retarget_sequence_steps(
    root: &Path,
    old: &Path,
    new: &Path,
) -> Result<usize, PersistenceError> {
    let dir = root.join(SEQUENCES_DIRNAME);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(PersistenceError::Read { path: dir, source }),
    };
    let read_err = |source| PersistenceError::Read {
        path: dir.clone(),
        source,
    };
    let mut rewritten = 0usize;
    for entry in entries {
        let path = entry.map_err(read_err)?.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(mut sequence) = load_sequence(&path) else {
            continue;
        };
        let mut changed = false;
        for step in &mut sequence.steps {
            if let Some(updated) = retarget_one(&step.endpoint, old, new) {
                step.endpoint = updated;
                changed = true;
                rewritten += 1;
            }
        }
        if changed {
            save_sequence(&path, &sequence)?;
        }
    }
    Ok(rewritten)
}

/// The repointed workspace-relative string for one step path, or `None` when it
/// does not reference `old`. An exact match (an endpoint) becomes `new`; a match
/// under `old` as a directory prefix (a collection) has that prefix swapped.
/// Rebuilt with `/` separators (the on-disk step convention), OS-independent.
fn retarget_one(step: &str, old: &Path, new: &Path) -> Option<String> {
    let step_path = Path::new(step);
    if step_path == old {
        return Some(to_rel_string(new));
    }
    if let Ok(rest) = step_path.strip_prefix(old) {
        return Some(to_rel_string(&new.join(rest)));
    }
    None
}

/// Joins a path's normal components with `/` (the workspace-relative step
/// convention), dropping any non-normal component defensively.
fn to_rel_string(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
