/// The version of `churl-core`, derived from the crate's `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod history;
pub mod model;
pub mod persistence;
