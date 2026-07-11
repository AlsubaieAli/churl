use super::*;

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
