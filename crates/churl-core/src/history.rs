//! Request history persisted in SQLite via `rusqlite` (bundled).
//!
//! High-churn state lives outside the workspace (which is a synced git repo) at
//! `<data_dir>/churl/state.sqlite` — see [`default_state_path`]. The schema is
//! managed by an idempotent migration runner keyed on `PRAGMA user_version`.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Ordered schema migrations. Migration `N` (1-based) runs when `user_version < N`,
/// inside a transaction that bumps `user_version` to `N` on success.
const MIGRATIONS: &[&str] = &[
    // 1: initial history table.
    "CREATE TABLE history (
        id INTEGER PRIMARY KEY,
        executed_at_ms INTEGER NOT NULL,
        method TEXT NOT NULL,
        url TEXT NOT NULL,
        status INTEGER,
        duration_ms INTEGER,
        endpoint_path TEXT
    );
    CREATE INDEX idx_history_executed_at ON history(executed_at_ms DESC);",
    // 2: recently-opened workspaces, for the quick-jump workspace picker (M7.2).
    // Recency lives here (SQLite state), never in the workspace files themselves.
    "CREATE TABLE workspaces (
        id INTEGER PRIMARY KEY,
        path TEXT UNIQUE NOT NULL,
        last_opened_ms INTEGER NOT NULL
    );
    CREATE INDEX idx_workspaces_last_opened ON workspaces(last_opened_ms DESC);",
];

/// Error opening or querying the history store.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The parent directory of the database file could not be created.
    #[error("failed to create state directory {path}: {source}")]
    CreateDir {
        /// Directory that failed to be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// An underlying SQLite operation failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Returns the default state database path (`<data_dir>/churl/state.sqlite`),
/// or `None` when the platform data directory cannot be determined.
pub fn default_state_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("churl").join("state.sqlite"))
}

/// A history entry about to be inserted (no id yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewHistoryEntry {
    /// Execution time as Unix milliseconds.
    pub executed_at_ms: i64,
    /// HTTP method string, e.g. `"GET"`.
    pub method: String,
    /// Requested URL after template substitution.
    pub url: String,
    /// Response status code; `None` when the request never completed.
    pub status: Option<u16>,
    /// Total request duration in milliseconds; `None` when unmeasured.
    pub duration_ms: Option<u64>,
    /// Workspace-relative path of the originating endpoint file, when any.
    pub endpoint_path: Option<String>,
}

/// A stored history entry, as returned by [`HistoryStore::recent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// SQLite rowid of the entry.
    pub id: i64,
    /// Execution time as Unix milliseconds.
    pub executed_at_ms: i64,
    /// HTTP method string, e.g. `"GET"`.
    pub method: String,
    /// Requested URL after template substitution.
    pub url: String,
    /// Response status code; `None` when the request never completed.
    pub status: Option<u16>,
    /// Total request duration in milliseconds; `None` when unmeasured.
    pub duration_ms: Option<u64>,
    /// Workspace-relative path of the originating endpoint file, when any.
    pub endpoint_path: Option<String>,
}

/// SQLite-backed store for executed-request history.
#[derive(Debug)]
pub struct HistoryStore {
    conn: Connection,
}

impl HistoryStore {
    /// Opens (creating if needed) the history database at `path`, creating parent
    /// directories and running any pending migrations. Reopening an up-to-date
    /// database is a no-op for the schema.
    pub fn open(path: &Path) -> Result<Self, HistoryError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| HistoryError::CreateDir {
                path: parent.to_owned(),
                source,
            })?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens an in-memory history store; intended for tests.
    pub fn in_memory() -> Result<Self, HistoryError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Wraps a connection and brings its schema up to date.
    fn from_connection(conn: Connection) -> Result<Self, HistoryError> {
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Applies every pending migration, each in its own transaction that also bumps
    /// `PRAGMA user_version`. Idempotent across reopens.
    fn migrate(&mut self) -> Result<(), HistoryError> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        for (index, sql) in MIGRATIONS.iter().enumerate() {
            let version = index as i64 + 1;
            if version <= current {
                continue;
            }
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// The current schema version (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64, HistoryError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Inserts a history entry and returns its rowid.
    pub fn insert(&self, entry: &NewHistoryEntry) -> Result<i64, HistoryError> {
        self.conn.execute(
            "INSERT INTO history (executed_at_ms, method, url, status, duration_ms, endpoint_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                entry.executed_at_ms,
                entry.method,
                entry.url,
                entry.status,
                // SQLite stores signed 64-bit integers only; saturate rather than wrap.
                entry
                    .duration_ms
                    .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX)),
                entry.endpoint_path,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Returns up to `limit` history entries, newest first (by execution time,
    /// breaking ties by descending id).
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, executed_at_ms, method, url, status, duration_ms, endpoint_path
             FROM history
             ORDER BY executed_at_ms DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                executed_at_ms: row.get(1)?,
                method: row.get(2)?,
                url: row.get(3)?,
                status: row.get(4)?,
                duration_ms: row
                    .get::<_, Option<i64>>(5)?
                    .map(|ms| u64::try_from(ms).unwrap_or_default()),
                endpoint_path: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Records that the workspace at `path` was opened at `now_ms`, inserting a
    /// new row or bumping the existing row's `last_opened_ms` (keyed on the
    /// UNIQUE `path`). Callers should pass a canonical absolute path so the same
    /// workspace is never duplicated under different relative spellings.
    pub fn touch_workspace(&self, path: &str, now_ms: i64) -> Result<(), HistoryError> {
        self.conn.execute(
            "INSERT INTO workspaces (path, last_opened_ms)
             VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET last_opened_ms = excluded.last_opened_ms",
            rusqlite::params![path, now_ms],
        )?;
        Ok(())
    }

    /// Returns up to `limit` workspace paths, most-recently-opened first.
    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<String>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM workspaces
             ORDER BY last_opened_ms DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at_ms: i64, url: &str) -> NewHistoryEntry {
        NewHistoryEntry {
            executed_at_ms: at_ms,
            method: "GET".into(),
            url: url.into(),
            status: Some(200),
            duration_ms: Some(42),
            endpoint_path: Some("users/get-user.toml".into()),
        }
    }

    #[test]
    fn insert_and_recent_order_and_limit() {
        let store = HistoryStore::in_memory().unwrap();
        let id1 = store.insert(&entry(1_000, "https://a.example")).unwrap();
        let id2 = store.insert(&entry(3_000, "https://b.example")).unwrap();
        let id3 = store.insert(&entry(2_000, "https://c.example")).unwrap();
        assert!(id1 < id2 && id2 < id3);

        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 3);
        let urls: Vec<&str> = recent.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://b.example",
                "https://c.example",
                "https://a.example"
            ]
        );

        let limited = store.recent(1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, id2);
        assert_eq!(limited[0].status, Some(200));
        assert_eq!(limited[0].duration_ms, Some(42));
    }

    #[test]
    fn nullable_fields_round_trip() {
        let store = HistoryStore::in_memory().unwrap();
        let sparse = NewHistoryEntry {
            executed_at_ms: 5,
            method: "POST".into(),
            url: "https://x.example".into(),
            status: None,
            duration_ms: None,
            endpoint_path: None,
        };
        store.insert(&sparse).unwrap();
        let got = &store.recent(1).unwrap()[0];
        assert_eq!(got.status, None);
        assert_eq!(got.duration_ms, None);
        assert_eq!(got.endpoint_path, None);
    }

    #[test]
    fn touch_workspace_inserts_and_recent_orders() {
        let store = HistoryStore::in_memory().unwrap();
        store.touch_workspace("/ws/a", 1_000).unwrap();
        store.touch_workspace("/ws/b", 3_000).unwrap();
        store.touch_workspace("/ws/c", 2_000).unwrap();

        let recent = store.recent_workspaces(10).unwrap();
        assert_eq!(recent, ["/ws/b", "/ws/c", "/ws/a"]);

        // Limit is respected.
        assert_eq!(store.recent_workspaces(2).unwrap(), ["/ws/b", "/ws/c"]);
    }

    #[test]
    fn touch_workspace_upserts_without_duplicating() {
        let store = HistoryStore::in_memory().unwrap();
        store.touch_workspace("/ws/a", 1_000).unwrap();
        store.touch_workspace("/ws/b", 2_000).unwrap();
        // Re-touch `a` with a newer timestamp: no new row, and it becomes newest.
        store.touch_workspace("/ws/a", 5_000).unwrap();

        let recent = store.recent_workspaces(10).unwrap();
        assert_eq!(recent, ["/ws/a", "/ws/b"], "no duplicate row; a is newest");

        // The UNIQUE(path) constraint means exactly one row per path.
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE path = '/ws/a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migrations_idempotent_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.sqlite");

        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        store.insert(&entry(1, "https://a.example")).unwrap();
        drop(store);

        // Reopen: migrations must not re-run or fail; data must survive.
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        store.insert(&entry(2, "https://b.example")).unwrap();
        assert_eq!(store.recent(10).unwrap().len(), 2);
    }
}
