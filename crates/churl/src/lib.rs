// The TUI threads churl-core's large `PersistenceError`/`InterchangeError` by
// value through its persistence calls; accept `result_large_err` crate-wide
// rather than boxing the core error types (see churl-core's crate root).
#![allow(clippy::result_large_err)]

// Re-export the tui module so integration tests can access `churl::tui::render`.
pub mod tui;
