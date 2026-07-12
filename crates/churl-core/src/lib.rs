// `PersistenceError` is churl-core's pervasive Result error and legitimately
// exceeds clippy's 128-byte `result_large_err` threshold. It is threaded by
// value across the persistence/interchange APIs rather than boxed — boxing
// fights thiserror's `#[from]` at every `?` site for no real gain on these
// cold error paths — so the lint is accepted crate-wide. (The exact size, and
// thus whether the lint fires, varies by clippy version; accepting it here
// keeps CI deterministic across toolchain versions.)
#![allow(clippy::result_large_err)]

/// The version of `churl-core`, derived from the crate's `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod auth;
pub mod config;
pub mod export;
pub mod history;
pub mod http;
pub mod import;
pub mod interchange;
pub mod load;
pub mod model;
pub mod persistence;
pub mod pin;
pub mod sequence;
pub mod template;
