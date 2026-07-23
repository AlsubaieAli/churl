use super::*;

/// Sorts endpoints by `seq`, then filename (the stable order used everywhere the
/// explorer and search show endpoints).
pub(super) fn sort_endpoints(endpoints: &mut [(PathBuf, Endpoint)]) {
    endpoints.sort_by(|(path_a, ep_a), (path_b, ep_b)| {
        ep_a.seq
            .cmp(&ep_b.seq)
            .then_with(|| path_a.file_name().cmp(&path_b.file_name()))
    });
}

/// Loads and deserializes a TOML file into `T`.
pub(super) fn load_value<T: DeserializeOwned>(path: &Path) -> Result<T, PersistenceError> {
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
pub(super) fn save_value<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistenceError> {
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
///
/// `pub` (re-exported by `persistence::mod`): the global-config settings writer
/// (`crate::config::save_defaults`, M8.5) reuses this same durable-write
/// primitive instead of duplicating it, and the `churl` binary's headless
/// `-o/--output` write path and TUI save-response-body gesture (M8.7) both call
/// it directly across the crate boundary for the exact same reason — one
/// durable-write primitive, never duplicated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
pub(super) fn normalize_table(table: &mut Table) {
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
