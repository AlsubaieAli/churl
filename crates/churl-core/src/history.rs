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
    // 3: load-run batch summaries (M7.5). A load run fires N copies of one
    // endpoint; recording one row per copy would flood the per-endpoint history
    // view, so a completed run writes exactly ONE summary row to this SEPARATE
    // table (never to `history`). Structural non-flooding: the per-endpoint
    // history query cannot see these rows.
    "CREATE TABLE load_batches (
        id INTEGER PRIMARY KEY,
        executed_at_ms INTEGER NOT NULL,
        url TEXT NOT NULL,
        endpoint_path TEXT,
        total INTEGER NOT NULL,
        concurrency INTEGER NOT NULL,
        ok_count INTEGER NOT NULL,
        fail_count INTEGER NOT NULL,
        error_count INTEGER NOT NULL,
        cancelled INTEGER NOT NULL DEFAULT 0,
        min_ms INTEGER,
        median_ms INTEGER,
        p95_ms INTEGER,
        max_ms INTEGER
    );
    CREATE INDEX idx_load_batches_executed_at ON load_batches(executed_at_ms DESC);",
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

/// A completed load-run summary about to be inserted into `load_batches` (M7.5).
/// Exactly one of these is written per completed (or cancelled) load run — never
/// per-request rows, so the per-endpoint history view is never flooded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadBatchSummary {
    /// Run time as Unix milliseconds.
    pub executed_at_ms: i64,
    /// The resolved target URL every copy was fired at.
    pub url: String,
    /// Workspace-relative path of the originating endpoint file, when any.
    pub endpoint_path: Option<String>,
    /// Number of copies fired.
    pub total: usize,
    /// Concurrency the run used.
    pub concurrency: usize,
    /// Count of `Ok` (< 400) outcomes.
    pub ok_count: usize,
    /// Count of `Failed` (>= 400) outcomes.
    pub fail_count: usize,
    /// Count of transport-error outcomes.
    pub error_count: usize,
    /// Whether the run was cancelled before all copies completed (partial summary).
    pub cancelled: bool,
    /// Minimum completed-request latency in ms, if any completed.
    pub min_ms: Option<u64>,
    /// Median (nearest-rank p50) completed-request latency in ms.
    pub median_ms: Option<u64>,
    /// 95th-percentile completed-request latency in ms.
    pub p95_ms: Option<u64>,
    /// Maximum completed-request latency in ms.
    pub max_ms: Option<u64>,
}

/// A stored load-batch summary, as returned by [`HistoryStore::recent_load_batches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadBatchEntry {
    /// SQLite rowid.
    pub id: i64,
    /// The inserted summary.
    pub summary: LoadBatchSummary,
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

    /// Inserts one load-run summary into the SEPARATE `load_batches` table
    /// (never `history`) and returns its rowid. Saturates each count to `i64`.
    pub fn insert_load_batch(&self, summary: &LoadBatchSummary) -> Result<i64, HistoryError> {
        let as_i64 = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        let ms_i64 = |ms: Option<u64>| ms.map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        self.conn.execute(
            "INSERT INTO load_batches
               (executed_at_ms, url, endpoint_path, total, concurrency,
                ok_count, fail_count, error_count, cancelled,
                min_ms, median_ms, p95_ms, max_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                summary.executed_at_ms,
                summary.url,
                summary.endpoint_path,
                as_i64(summary.total),
                as_i64(summary.concurrency),
                as_i64(summary.ok_count),
                as_i64(summary.fail_count),
                as_i64(summary.error_count),
                i64::from(summary.cancelled),
                ms_i64(summary.min_ms),
                ms_i64(summary.median_ms),
                ms_i64(summary.p95_ms),
                ms_i64(summary.max_ms),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Returns up to `limit` load-batch summaries, newest first.
    pub fn recent_load_batches(&self, limit: usize) -> Result<Vec<LoadBatchEntry>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, executed_at_ms, url, endpoint_path, total, concurrency,
                    ok_count, fail_count, error_count, cancelled,
                    min_ms, median_ms, p95_ms, max_ms
             FROM load_batches
             ORDER BY executed_at_ms DESC, id DESC
             LIMIT ?1",
        )?;
        let to_usize = |n: i64| usize::try_from(n).unwrap_or_default();
        let to_ms = |v: Option<i64>| v.map(|ms| u64::try_from(ms).unwrap_or_default());
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(LoadBatchEntry {
                id: row.get(0)?,
                summary: LoadBatchSummary {
                    executed_at_ms: row.get(1)?,
                    url: row.get(2)?,
                    endpoint_path: row.get(3)?,
                    total: to_usize(row.get(4)?),
                    concurrency: to_usize(row.get(5)?),
                    ok_count: to_usize(row.get(6)?),
                    fail_count: to_usize(row.get(7)?),
                    error_count: to_usize(row.get(8)?),
                    cancelled: row.get::<_, i64>(9)? != 0,
                    min_ms: to_ms(row.get(10)?),
                    median_ms: to_ms(row.get(11)?),
                    p95_ms: to_ms(row.get(12)?),
                    max_ms: to_ms(row.get(13)?),
                },
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

    fn batch(at_ms: i64, url: &str, cancelled: bool) -> LoadBatchSummary {
        LoadBatchSummary {
            executed_at_ms: at_ms,
            url: url.into(),
            endpoint_path: Some("users/list.toml".into()),
            total: 50,
            concurrency: 10,
            ok_count: 44,
            fail_count: 5,
            error_count: 1,
            cancelled,
            min_ms: Some(12),
            median_ms: Some(45),
            p95_ms: Some(120),
            max_ms: Some(210),
        }
    }

    #[test]
    fn load_batch_insert_and_read_back() {
        let store = HistoryStore::in_memory().unwrap();
        let id = store
            .insert_load_batch(&batch(1_000, "https://api.test/users", false))
            .unwrap();
        assert!(id > 0);
        let got = store.recent_load_batches(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, id);
        assert_eq!(
            got[0].summary,
            batch(1_000, "https://api.test/users", false)
        );
    }

    #[test]
    fn load_batch_null_percentiles_round_trip() {
        // An all-errored run has counts but no latencies.
        let store = HistoryStore::in_memory().unwrap();
        let summary = LoadBatchSummary {
            executed_at_ms: 5,
            url: "https://api.test/down".into(),
            endpoint_path: None,
            total: 3,
            concurrency: 3,
            ok_count: 0,
            fail_count: 0,
            error_count: 3,
            cancelled: false,
            min_ms: None,
            median_ms: None,
            p95_ms: None,
            max_ms: None,
        };
        store.insert_load_batch(&summary).unwrap();
        let got = &store.recent_load_batches(1).unwrap()[0].summary;
        assert_eq!(got, &summary);
        assert_eq!(got.min_ms, None);
        assert_eq!(got.endpoint_path, None);
    }

    #[test]
    fn cancelled_flag_round_trips() {
        let store = HistoryStore::in_memory().unwrap();
        store
            .insert_load_batch(&batch(1, "https://api.test/a", true))
            .unwrap();
        assert!(store.recent_load_batches(1).unwrap()[0].summary.cancelled);
    }

    #[test]
    fn load_batches_never_appear_in_per_endpoint_history() {
        // The structural non-flooding guarantee: batch summaries live in a
        // separate table, so the per-endpoint history query returns ONLY history
        // rows even after load batches are recorded.
        let store = HistoryStore::in_memory().unwrap();
        store.insert(&entry(1_000, "https://a.example")).unwrap();
        store
            .insert_load_batch(&batch(2_000, "https://batch.example", false))
            .unwrap();
        store
            .insert_load_batch(&batch(3_000, "https://batch2.example", true))
            .unwrap();
        store.insert(&entry(4_000, "https://b.example")).unwrap();

        // History has exactly the two single-request rows — no batch URLs.
        let recent = store.recent(100).unwrap();
        assert_eq!(recent.len(), 2, "load batches must not enter history");
        let urls: Vec<&str> = recent.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, ["https://b.example", "https://a.example"]);
        assert!(!urls.iter().any(|u| u.contains("batch")));

        // And the batches are all present in their own table.
        assert_eq!(store.recent_load_batches(100).unwrap().len(), 2);
    }

    #[test]
    fn migration_3_applies_from_a_v2_db() {
        // Build a v2 database by hand (history + workspaces, user_version = 2),
        // then open through the store: migration 3 must add load_batches without
        // disturbing the existing data.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
            conn.execute(
                "INSERT INTO history (executed_at_ms, method, url) VALUES (1, 'GET', 'https://old.example')",
                [],
            )
            .unwrap();
        }
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        // Pre-existing history survived.
        assert_eq!(store.recent(10).unwrap().len(), 1);
        // The new table works.
        store
            .insert_load_batch(&batch(9, "https://api.test/x", false))
            .unwrap();
        assert_eq!(store.recent_load_batches(10).unwrap().len(), 1);
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
