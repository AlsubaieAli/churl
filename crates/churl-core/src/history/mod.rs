//! Request history persisted in SQLite via `rusqlite` (bundled).
//!
//! High-churn state lives outside the workspace (which is a synced git repo) at
//! `<data_dir>/churl/state.sqlite` — see [`default_state_path`]. The schema is
//! managed by an idempotent migration runner keyed on `PRAGMA user_version`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, TransactionBehavior};

/// How long a writer waits for a busy database before erroring `SQLITE_BUSY`
/// (R1 D2). A concurrent churl process holding the write lock (e.g. migrating)
/// releases it in milliseconds, so 5 s is generous headroom without hanging the
/// UI indefinitely on a genuinely stuck lock.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Retained-row cap per history table (R1 D4b). Every insert prunes the table
/// back to the newest `HISTORY_ROW_CAP` rows so an arbitrarily long session
/// cannot grow `state.sqlite` without bound. Reads (`recent`) are unaffected —
/// they already `LIMIT` far below this. Applied independently to `history` and
/// `load_batches` (the `workspaces` table is naturally bounded: one row per
/// distinct workspace path).
const HISTORY_ROW_CAP: i64 = 10_000;

mod reads;
mod schema;
mod writes;

use schema::MIGRATIONS;

/// Error opening or querying the history store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// `PRAGMA journal_mode=WAL` did not take — the filesystem silently fell back
    /// to another journal mode (some network/virtual filesystems reject WAL). We
    /// fail loud rather than run in a mode we didn't ask for (Constitution).
    #[error(
        "journal_mode=WAL was rejected (fell back to {actual}); refusing to run in an unexpected mode"
    )]
    WalUnavailable {
        /// The journal mode SQLite actually reported after the PRAGMA.
        actual: String,
    },
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
    /// Arithmetic-mean completed-request latency in ms.
    pub mean_ms: Option<u64>,
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
        // File-backed DBs must land in WAL mode (durable + concurrent readers);
        // an in-memory DB has no journal file, so WAL is not asserted there.
        Self::from_connection(Connection::open(path)?, true)
    }

    /// Opens an in-memory history store; intended for tests. WAL is not asserted
    /// (in-memory databases have no WAL journal), but `busy_timeout` and the
    /// migration lock are still applied so tests exercise the same path.
    pub fn in_memory() -> Result<Self, HistoryError> {
        Self::from_connection(Connection::open_in_memory()?, false)
    }

    /// Wraps a connection, applies the durability/concurrency PRAGMAs, and brings
    /// its schema up to date. `expect_wal` asserts `journal_mode=WAL` actually
    /// took (file-backed DBs only — an in-memory DB legitimately reports `memory`).
    fn from_connection(conn: Connection, expect_wal: bool) -> Result<Self, HistoryError> {
        // 1. busy_timeout FIRST, so the very next statement (incl. the migration
        //    lock) waits on a busy DB instead of erroring SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
        // 2. WAL: durable and concurrent-reader friendly. `journal_mode` returns
        //    the mode that actually took — fail loud if the FS silently refused.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if expect_wal && !mode.eq_ignore_ascii_case("wal") {
            return Err(HistoryError::WalUnavailable { actual: mode });
        }
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Brings the schema up to date under a single write lock, so concurrent
    /// openers can never race the migration (R1 D2).
    ///
    /// The whole run happens inside one `BEGIN IMMEDIATE` transaction (a write
    /// lock taken up front): `user_version` is re-read *inside* the lock, only
    /// still-pending migrations are applied, `user_version` is bumped, then the
    /// transaction commits — all-or-nothing. A second process blocks on the lock
    /// (up to `busy_timeout`), then re-reads the now-current version and no-ops.
    fn migrate(&mut self) -> Result<(), HistoryError> {
        // IMMEDIATE grabs the RESERVED write lock at BEGIN, before we read
        // `user_version` — so two concurrent migrators serialize on the lock
        // rather than both reading a stale version and double-applying.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        for (index, sql) in MIGRATIONS.iter().enumerate() {
            let version = index as i64 + 1;
            if version <= current {
                continue;
            }
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
        }
        // One commit for the whole run; on any error above the tx drops and rolls
        // back automatically (all-or-nothing).
        tx.commit()?;
        Ok(())
    }

    /// The current schema version (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64, HistoryError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
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
            mean_ms: Some(60),
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
        // mean_ms round-trips (regression guard: added after the initial DDL).
        assert_eq!(got[0].summary.mean_ms, Some(60));
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
            mean_ms: None,
        };
        store.insert_load_batch(&summary).unwrap();
        let got = &store.recent_load_batches(1).unwrap()[0].summary;
        assert_eq!(got, &summary);
        assert_eq!(got.min_ms, None);
        assert_eq!(got.mean_ms, None);
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

    /// The number of columns named `mean_ms` on `load_batches` (must be exactly 1
    /// — regression guard against `mean_ms` living in both migration 3's CREATE
    /// and migration 4's ALTER, which would double-add on a fresh DB and fail).
    fn mean_ms_column_count(store: &HistoryStore) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('load_batches') WHERE name = 'mean_ms'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn fresh_db_reaches_latest_with_mean_column_once() {
        // A fresh DB runs every migration v0→latest; `mean_ms` must be present
        // exactly once (only in migration 4's ALTER, never in migration 3).
        let store = HistoryStore::in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(
            mean_ms_column_count(&store),
            1,
            "mean_ms added exactly once"
        );
        store
            .insert_load_batch(&batch(1, "https://api.test/x", false))
            .unwrap();
        assert_eq!(
            store.recent_load_batches(1).unwrap()[0].summary.mean_ms,
            Some(60)
        );
    }

    #[test]
    fn old_v3_db_without_mean_ms_migrates_to_add_it() {
        // Reproduces the real-binary bug: a DB already at the OLD user_version = 3
        // (a `load_batches` table WITHOUT `mean_ms`, the pre-fix DDL). Opening it
        // must run migration 4's ALTER so `insert_load_batch` (which writes
        // `mean_ms`) succeeds — editing migration 3 in place would NOT have fixed
        // this, since a v3 DB skips migration 3 forever.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            // Migrations 1 + 2 verbatim, then the OLD migration-3 load_batches DDL
            // (no `mean_ms` column), and stamp user_version = 3.
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute_batch(
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
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3i64).unwrap();
            // Confirm the starting DB genuinely lacks the column.
            let cols: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('load_batches') WHERE name = 'mean_ms'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(cols, 0, "the old v3 DB must not have mean_ms yet");
        }

        // Opening applies migration 4 (only) → mean_ms exists and inserts work.
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(mean_ms_column_count(&store), 1);
        // The insert that broke the real binary now succeeds and round-trips mean.
        store
            .insert_load_batch(&batch(7, "https://api.test/y", false))
            .unwrap();
        assert_eq!(
            store.recent_load_batches(1).unwrap()[0].summary.mean_ms,
            Some(60)
        );
    }

    // ---- R1 D4b: history pruning ------------------------------------------

    /// Counts rows in a table on the store's connection.
    fn row_count(store: &HistoryStore, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// Inserting past the cap prunes back to exactly the cap, keeping the newest
    /// rows (prune-on-insert). Overshoots the cap by a handful to keep the test
    /// cheap while still crossing the boundary.
    #[test]
    fn history_insert_prunes_to_cap_keeping_newest() {
        let store = HistoryStore::in_memory().unwrap();
        let overshoot = 5i64;
        let total = HISTORY_ROW_CAP + overshoot;
        for i in 0..total {
            // executed_at_ms increases with i so "newest" is unambiguous.
            store.insert(&entry(i, &format!("https://ex/{i}"))).unwrap();
        }
        assert_eq!(
            row_count(&store, "history"),
            HISTORY_ROW_CAP,
            "history capped at HISTORY_ROW_CAP"
        );
        // The newest row survived; the oldest `overshoot` rows were pruned.
        let recent = store.recent(1).unwrap();
        assert_eq!(recent[0].url, format!("https://ex/{}", total - 1));
        // recent() is unaffected by pruning (still returns the newest window).
        assert_eq!(store.recent(10).unwrap().len(), 10);
    }

    /// The same cap applies independently to `load_batches`.
    #[test]
    fn load_batches_insert_prunes_to_cap() {
        let store = HistoryStore::in_memory().unwrap();
        for i in 0..(HISTORY_ROW_CAP + 3) {
            store
                .insert_load_batch(&batch(i, &format!("https://b/{i}"), false))
                .unwrap();
        }
        assert_eq!(row_count(&store, "load_batches"), HISTORY_ROW_CAP);
        // history untouched by load-batch pruning.
        assert_eq!(row_count(&store, "history"), 0);
    }

    // ---- R1 D2: WAL + busy_timeout + migration race guard -----------------

    /// A file-backed store lands in WAL mode with a non-zero busy_timeout after
    /// open (both PRAGMAs read back as applied).
    #[test]
    fn open_sets_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let store = HistoryStore::open(&path).unwrap();

        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(mode.eq_ignore_ascii_case("wal"), "journal_mode = {mode}");

        let timeout: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, i64::from(BUSY_TIMEOUT_MS), "busy_timeout applied");
        assert!(timeout > 0);

        // A WAL DB leaves a `-wal` sidecar next to the file.
        assert!(
            path.with_extension("sqlite-wal").exists()
                || std::fs::read_dir(dir.path())
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with("-wal")),
            "WAL sidecar present"
        );
    }

    /// The migration race guard: opening the SAME file twice (serialised, which
    /// is the invariant we can assert deterministically in-test) must leave the
    /// schema at exactly the latest version with no partial/duplicate migration —
    /// the second open re-reads the current version under the lock and no-ops.
    #[test]
    fn second_open_no_ops_migrations_and_reaches_latest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");

        let first = HistoryStore::open(&path).unwrap();
        assert_eq!(first.schema_version().unwrap(), MIGRATIONS.len() as i64);
        first.insert(&entry(1, "https://a.example")).unwrap();
        drop(first);

        // Reopen: migrate() re-reads user_version INSIDE the IMMEDIATE tx and
        // applies nothing. Schema intact, data survived, exactly one mean_ms.
        let second = HistoryStore::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(mean_ms_column_count(&second), 1, "no duplicate migration");
        assert_eq!(second.recent(10).unwrap().len(), 1, "data intact");
    }

    /// Two live connections to the same file: the first holds a store; a second
    /// `open` succeeds (WAL + busy_timeout let it through) and both see the latest
    /// schema. Exercises the concurrent-writer path without flaky threading.
    #[test]
    fn concurrent_connections_share_latest_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");

        let a = HistoryStore::open(&path).unwrap();
        let b = HistoryStore::open(&path).unwrap();
        assert_eq!(a.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(b.schema_version().unwrap(), MIGRATIONS.len() as i64);

        // Both can write; a busy retry under WAL never errors SQLITE_BUSY here.
        a.insert(&entry(1, "https://a.example")).unwrap();
        b.insert(&entry(2, "https://b.example")).unwrap();
        // Each connection sees both rows (WAL readers see committed writes).
        assert_eq!(a.recent(10).unwrap().len(), 2);
        assert_eq!(b.recent(10).unwrap().len(), 2);
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
