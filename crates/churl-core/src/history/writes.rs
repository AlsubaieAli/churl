use super::{HISTORY_ROW_CAP, HistoryError, HistoryStore, LoadBatchSummary, NewHistoryEntry};

impl HistoryStore {
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
        let id = self.conn.last_insert_rowid();
        self.prune("history")?;
        Ok(id)
    }

    /// Prunes `table` back to its newest [`HISTORY_ROW_CAP`] rows by rowid (rowid
    /// is monotonic with insertion order for these INTEGER-PRIMARY-KEY tables, so
    /// "newest" = highest rowid).
    ///
    /// Cheap and index-friendly: it deletes the closed range `id <= max_id - CAP`
    /// in one shot (a range scan on the PK), touching nothing when the table is
    /// under the cap — no per-insert full-table subquery. Because ids only ever
    /// grow, `max_id - CAP` is exactly the retain boundary regardless of past
    /// deletions.
    fn prune(&self, table: &str) -> Result<(), HistoryError> {
        // `table` is a compile-time-constant literal at every call site, never
        // user input — safe to interpolate (rusqlite can't bind an identifier).
        let max_id: Option<i64> =
            self.conn
                .query_row(&format!("SELECT MAX(id) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        let Some(max_id) = max_id else {
            return Ok(());
        };
        let cutoff = max_id - HISTORY_ROW_CAP;
        if cutoff < 1 {
            return Ok(()); // still under the cap
        }
        self.conn
            .execute(&format!("DELETE FROM {table} WHERE id <= ?1"), [cutoff])?;
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
                min_ms, median_ms, p95_ms, max_ms, mean_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                ms_i64(summary.mean_ms),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.prune("load_batches")?;
        Ok(id)
    }
}
