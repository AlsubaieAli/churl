use super::{HistoryEntry, HistoryError, HistoryStore, LoadBatchEntry, LoadBatchSummary};

impl HistoryStore {
    /// Returns up to `limit` history entries, newest first (by execution time,
    /// breaking ties by descending id).
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, executed_at_ms, method, url, status, duration_ms, endpoint_path
             FROM history
             ORDER BY executed_at_ms DESC, id DESC
             LIMIT ?1",
        )?;
        // Saturate the row cap into SQLite's i64: a display limit never approaches
        // i64::MAX, and an over-large one still means "no practical ceiling".
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], |row| {
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

    /// Returns up to `limit` load-batch summaries, newest first.
    pub fn recent_load_batches(&self, limit: usize) -> Result<Vec<LoadBatchEntry>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, executed_at_ms, url, endpoint_path, total, concurrency,
                    ok_count, fail_count, error_count, cancelled,
                    min_ms, median_ms, p95_ms, max_ms, mean_ms
             FROM load_batches
             ORDER BY executed_at_ms DESC, id DESC
             LIMIT ?1",
        )?;
        let to_usize = |n: i64| usize::try_from(n).unwrap_or_default();
        let to_ms = |v: Option<i64>| v.map(|ms| u64::try_from(ms).unwrap_or_default());
        // Saturate the row cap into SQLite's i64 (see `recent`).
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], |row| {
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
                    mean_ms: to_ms(row.get(14)?),
                },
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns the stored cookie-jar JSON blob for the workspace at `workspace`
    /// (a canonicalized root path), or `None` when none is stored yet. The blob
    /// is fed to [`crate::cookies::ChurlCookieJar::load_json`] on workspace open.
    pub fn cookie_jar(&self, workspace: &str) -> Result<Option<String>, HistoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT jar_json FROM cookies WHERE workspace = ?1")?;
        let mut rows = stmt.query([workspace])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Returns up to `limit` workspace paths, most-recently-opened first.
    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<String>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM workspaces
             ORDER BY last_opened_ms DESC
             LIMIT ?1",
        )?;
        // Saturate the row cap into SQLite's i64 (see `recent`).
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}
