//! Off-UI-thread cookie-jar persistence.
//!
//! [`CookieJarWriter`] owns a dedicated OS thread with its **own** [`HistoryStore`]
//! connection to `state.sqlite` and drains coalesced jar snapshots to it, so the
//! blocking `conn.execute` (which can wait up to the WAL `busy_timeout` under
//! cross-process write-lock contention) never runs on the UI thread. The TUI hands
//! over only a `(workspace, jar_json, updated_at)` snapshot — no SQLite type
//! crosses the boundary.
//!
//! Invariants (each is exercised by a test in this module):
//! - **No lost writes on quit.** [`CookieJarWriter::shutdown`] (and `Drop`) drains
//!   every still-pending snapshot before the thread exits and joins it, so the last
//!   update a session made is durable. `shutdown` *returns* the final write error
//!   (if any) so the quit path can surface it after the terminal is restored,
//!   rather than losing it (`take_error` is only polled during a running session).
//! - **Final state wins.** The queue is a `workspace → latest snapshot` map, so
//!   rapid successive updates coalesce to the newest one; the writer thread is the
//!   sole writer and processes drains in order, so an older snapshot can never land
//!   after a newer one.
//! - **Bounded.** The map holds at most one entry per workspace key (in practice
//!   one), so a response flood cannot grow it without bound.
//! - **No clobber on failure.** Serialization happens on the UI thread *before*
//!   enqueue (a failure skips the enqueue entirely — see `App::persist_cookie_jar`),
//!   so the writer only ever receives a good blob; a failed [`CookieSink::write`]
//!   (a single atomic upsert) leaves the prior stored blob intact, and its error is
//!   surfaced via [`CookieJarWriter::take_error`] / the [`CookieJarWriter::shutdown`]
//!   return. Fault-injected by a failing [`CookieSink`] in the tests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::{HistoryError, HistoryStore};

/// The blocking write the writer thread performs. Abstracted over so tests can
/// inject a failing sink to exercise the "no clobber on failure" invariant; in
/// production it is always a [`HistoryStore`] on its own connection.
pub(crate) trait CookieSink: Send {
    /// Upserts the jar blob for `workspace`. A single atomic write: on error the
    /// prior stored blob is left intact.
    fn write(&self, workspace: &str, jar_json: &str, updated_at: i64) -> Result<(), String>;
}

impl CookieSink for HistoryStore {
    fn write(&self, workspace: &str, jar_json: &str, updated_at: i64) -> Result<(), String> {
        self.save_cookie_jar(workspace, jar_json, updated_at)
            .map_err(|err| err.to_string())
    }
}

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
        Self::from_sink(Box::new(store))
    }

    /// Spawns the writer thread over an arbitrary [`CookieSink`]. `spawn` is the
    /// production entry (a `HistoryStore` sink); tests inject other sinks.
    fn from_sink(sink: Box<dyn CookieSink>) -> Result<Self, HistoryError> {
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
            .spawn(move || run(&worker_shared, sink.as_ref()))
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

    /// Flushes every pending snapshot, joins the writer thread, and returns the
    /// final write error (if any) so the quit path can surface it *after* the
    /// terminal is restored — `take_error` polling stops once the session ends, so
    /// a failed final flush would otherwise be lost. Idempotent: `Drop` also calls
    /// it (discarding the return), so an unflushed writer is never left dangling.
    pub fn shutdown(&mut self) -> Option<String> {
        let handle = self.handle.take()?;
        {
            let mut state = self.lock();
            state.shutdown = true;
        }
        self.shared.signal.notify_one();
        // Join so we do not return until the queue has drained to disk.
        let _ = handle.join();
        // Report any error from the drained writes (incl. the final flush).
        self.lock().last_error.take()
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
        // Backstop flush; a caller that wants the error calls `shutdown` explicitly.
        let _ = self.shutdown();
    }
}

/// The writer loop: wait for work, drain the coalesced snapshots to the sink,
/// repeat until shutdown *and* the queue is empty (so no pending write is dropped).
fn run(shared: &Shared, sink: &dyn CookieSink) {
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
            if let Err(err) = sink.write(&workspace, &snapshot.jar_json, snapshot.updated_at) {
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.last_error = Some(err);
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

    /// A sink whose write always fails without touching storage — models a write
    /// that errors out (SQLite's upsert is atomic, so a failed write leaves the
    /// prior stored blob untouched).
    struct AlwaysFail;
    impl CookieSink for AlwaysFail {
        fn write(&self, _workspace: &str, _jar_json: &str, _updated_at: i64) -> Result<(), String> {
            Err("injected write failure".to_owned())
        }
    }

    #[test]
    fn shutdown_flushes_the_last_write_no_loss_on_quit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        // Materialize the schema first (as the app does before spawning).
        HistoryStore::open(&path).unwrap();

        let mut writer = CookieJarWriter::spawn(&path).unwrap();
        writer.enqueue("/ws/a".to_owned(), "{\"v\":1}".to_owned(), 1_000);
        // Quit immediately: shutdown must drain the pending write before joining,
        // and report no error on a healthy DB.
        assert!(writer.shutdown().is_none());

        assert_eq!(stored(&path, "/ws/a").as_deref(), Some("{\"v\":1}"));
    }

    #[test]
    fn failed_write_is_surfaced_and_never_clobbers_the_prior_blob() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        HistoryStore::open(&path).unwrap();

        // Persist a good blob normally.
        let mut good = CookieJarWriter::spawn(&path).unwrap();
        good.enqueue("/ws/a".to_owned(), "{\"good\":1}".to_owned(), 1);
        assert!(good.shutdown().is_none(), "the good write must not error");
        assert_eq!(stored(&path, "/ws/a").as_deref(), Some("{\"good\":1}"));

        // A second writer whose sink always fails tries to overwrite it.
        let mut failing = CookieJarWriter::from_sink(Box::new(AlwaysFail)).unwrap();
        failing.enqueue("/ws/a".to_owned(), "{\"BAD\":2}".to_owned(), 2);
        let err = failing.shutdown();

        // (i) the failure is surfaced (never silently swallowed)...
        assert!(err.is_some(), "a failed write must be reported by shutdown");
        // (ii) ...and the prior good blob is intact — no clobber.
        assert_eq!(
            stored(&path, "/ws/a").as_deref(),
            Some("{\"good\":1}"),
            "a failed write must never overwrite a good stored blob"
        );
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
