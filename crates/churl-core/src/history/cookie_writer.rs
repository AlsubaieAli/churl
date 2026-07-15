//! Off-UI-thread cookie-jar persistence.
//!
//! [`CookieJarWriter`] owns a dedicated OS thread with its **own** [`HistoryStore`]
//! connection to `state.sqlite` and drains coalesced jar snapshots to it, so the
//! blocking `conn.execute` (which can wait up to the WAL `busy_timeout` under
//! cross-process write-lock contention) never runs on the UI thread. The TUI hands
//! over only a `(workspace, jar_json, updated_at)` snapshot — no SQLite type
//! crosses the boundary.
//!
//! Invariants (each is exercised by a test):
//! - **No lost writes on quit.** [`CookieJarWriter::shutdown`] (and `Drop`) drains
//!   every still-pending snapshot before the thread exits and joins it, so the last
//!   update a session made is durable.
//! - **Final state wins.** The queue is a `workspace → latest snapshot` map, so
//!   rapid successive updates coalesce to the newest one; the writer thread is the
//!   sole writer and processes drains in order, so an older snapshot can never land
//!   after a newer one.
//! - **Bounded.** The map holds at most one entry per workspace key (in practice
//!   one), so a response flood cannot grow it without bound.
//! - **No clobber on failure.** Serialization happens on the UI thread *before*
//!   enqueue (a failure skips the enqueue entirely — see `App::persist_cookie_jar`),
//!   so the writer only ever receives a good blob; a failed `save_cookie_jar` is an
//!   atomic upsert that leaves the prior stored blob intact, and its error is
//!   surfaced via [`CookieJarWriter::take_error`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::{HistoryError, HistoryStore};

/// A pending write: the latest serialized jar for a workspace plus its timestamp.
#[derive(Clone)]
struct Snapshot {
    jar_json: String,
    updated_at: i64,
}

/// Shared state between the enqueuing (UI) thread and the writer thread.
struct Shared {
    state: Mutex<WriterState>,
    /// Signalled on every enqueue and on shutdown.
    signal: Condvar,
}

struct WriterState {
    /// `workspace key → latest pending snapshot`. Coalescing + bounded by
    /// construction: a second enqueue for the same key overwrites the first.
    pending: HashMap<String, Snapshot>,
    /// Set once on shutdown; the writer drains what remains, then exits.
    shutdown: bool,
    /// The most recent write error, surfaced to the UI on its next persist call.
    last_error: Option<String>,
}

/// Persists cookie-jar snapshots off the UI thread. Drop flushes and joins.
pub struct CookieJarWriter {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl CookieJarWriter {
    /// Spawns the writer with its own [`HistoryStore`] connection to `path`. The
    /// second connection is harmless: WAL mode is already set on the file and the
    /// migration runner re-reads the current schema version and no-ops.
    pub fn spawn(path: &Path) -> Result<Self, HistoryError> {
        let store = HistoryStore::open(path)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(WriterState {
                pending: HashMap::new(),
                shutdown: false,
                last_error: None,
            }),
            signal: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("churl-cookie-writer".to_owned())
            .spawn(move || run(&worker_shared, &store))
            .map_err(|source| HistoryError::WriterSpawn { source })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    /// Queues the latest jar snapshot for `workspace`, coalescing with any pending
    /// snapshot for the same key. Never blocks on the database — only briefly on
    /// the in-memory queue lock.
    pub fn enqueue(&self, workspace: String, jar_json: String, updated_at: i64) {
        {
            let mut state = self.lock();
            state.pending.insert(
                workspace,
                Snapshot {
                    jar_json,
                    updated_at,
                },
            );
        }
        self.shared.signal.notify_one();
    }

    /// Takes the most recent write error (clearing it), for the UI to surface. A
    /// serialize/write failure never clobbers a good stored blob; this only
    /// reports it loudly.
    pub fn take_error(&self) -> Option<String> {
        self.lock().last_error.take()
    }

    /// Flushes every pending snapshot and joins the writer thread. Idempotent —
    /// `Drop` calls it too, so an explicit call on the quit path guarantees the
    /// last write is durable before the process exits.
    pub fn shutdown(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        {
            let mut state = self.lock();
            state.shutdown = true;
        }
        self.shared.signal.notify_one();
        // Join so we do not return until the queue has drained to disk.
        let _ = handle.join();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        // Recover from a poisoned lock rather than panicking: a cookie write must
        // never take down the app (mirrors the jar's own poison recovery).
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for CookieJarWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The writer loop: wait for work, drain the coalesced snapshots to SQLite, repeat
/// until shutdown *and* the queue is empty (so no pending write is dropped).
fn run(shared: &Shared, store: &HistoryStore) {
    loop {
        let batch: Vec<(String, Snapshot)> = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.pending.is_empty() && !state.shutdown {
                state = shared
                    .signal
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.pending.is_empty() && state.shutdown {
                return;
            }
            state.pending.drain().collect()
        };
        // Write outside the lock so enqueues never block on the database.
        for (workspace, snapshot) in batch {
            if let Err(err) =
                store.save_cookie_jar(&workspace, &snapshot.jar_json, snapshot.updated_at)
            {
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.last_error = Some(err.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;

    /// Reads the stored blob for a workspace via a fresh store on the same file.
    fn stored(path: &Path, workspace: &str) -> Option<String> {
        HistoryStore::open(path)
            .unwrap()
            .cookie_jar(workspace)
            .unwrap()
    }

    #[test]
    fn shutdown_flushes_the_last_write_no_loss_on_quit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        // Materialize the schema first (as the app does before spawning).
        HistoryStore::open(&path).unwrap();

        let mut writer = CookieJarWriter::spawn(&path).unwrap();
        writer.enqueue("/ws/a".to_owned(), "{\"v\":1}".to_owned(), 1_000);
        // Quit immediately: shutdown must drain the pending write before joining.
        writer.shutdown();

        assert_eq!(stored(&path, "/ws/a").as_deref(), Some("{\"v\":1}"));
    }

    #[test]
    fn rapid_updates_coalesce_to_final_state_in_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        HistoryStore::open(&path).unwrap();

        let mut writer = CookieJarWriter::spawn(&path).unwrap();
        for i in 1..=50 {
            writer.enqueue("/ws/a".to_owned(), format!("{{\"v\":{i}}}"), i);
        }
        writer.shutdown();

        // The newest snapshot wins...
        assert_eq!(stored(&path, "/ws/a").as_deref(), Some("{\"v\":50}"));
        // ...and coalescing (plus the upsert) left exactly one row: bounded, no
        // duplicates.
        let store = HistoryStore::open(&path).unwrap();
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cookies WHERE workspace = '/ws/a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn distinct_workspaces_each_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        HistoryStore::open(&path).unwrap();

        let mut writer = CookieJarWriter::spawn(&path).unwrap();
        writer.enqueue("/ws/a".to_owned(), "{\"a\":true}".to_owned(), 1);
        writer.enqueue("/ws/b".to_owned(), "{\"b\":true}".to_owned(), 2);
        writer.shutdown();

        assert_eq!(stored(&path, "/ws/a").as_deref(), Some("{\"a\":true}"));
        assert_eq!(stored(&path, "/ws/b").as_deref(), Some("{\"b\":true}"));
    }

    #[test]
    fn drop_without_explicit_shutdown_still_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        HistoryStore::open(&path).unwrap();

        {
            let writer = CookieJarWriter::spawn(&path).unwrap();
            writer.enqueue("/ws/a".to_owned(), "{\"dropped\":true}".to_owned(), 9);
            // Drop at end of scope must flush.
        }
        assert_eq!(
            stored(&path, "/ws/a").as_deref(),
            Some("{\"dropped\":true}")
        );
    }
}
