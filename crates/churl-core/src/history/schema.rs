/// Ordered schema migrations. Migration `N` (1-based) runs when `user_version < N`,
/// inside a transaction that bumps `user_version` to `N` on success.
pub(super) const MIGRATIONS: &[&str] = &[
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
    // 2: recently-opened workspaces, for the quick-jump workspace picker.
    // Recency lives here (SQLite state), never in the workspace files themselves.
    "CREATE TABLE workspaces (
        id INTEGER PRIMARY KEY,
        path TEXT UNIQUE NOT NULL,
        last_opened_ms INTEGER NOT NULL
    );
    CREATE INDEX idx_workspaces_last_opened ON workspaces(last_opened_ms DESC);",
    // 3: load-run batch summaries. A load run fires N copies of one
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
    // 4: add the mean-latency column to load_batches. Appended
    // as an ALTER rather than editing migration 3's CREATE (migrations are
    // append-only): a DB already at v3 — e.g. from an earlier dev build of this
    // branch — skips migration 3 forever, so the column must arrive via its own
    // migration to land on both fresh (v2→v3→v4) and already-v3 (→v4) databases.
    "ALTER TABLE load_batches ADD COLUMN mean_ms INTEGER;",
    // 5: per-workspace persistent cookie jars. One row per workspace (keyed by
    // its canonicalized root path); `jar_json` is the persistent-cookies-only
    // blob serialized by `ChurlCookieJar::to_json`. Local-only state, never in
    // the synced workspace files — parity with history.
    "CREATE TABLE cookies (
        workspace TEXT PRIMARY KEY,
        jar_json TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );",
];
