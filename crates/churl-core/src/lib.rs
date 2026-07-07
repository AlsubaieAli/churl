/// The version of `churl-core`, derived from the crate's `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod auth;
pub mod config;
pub mod export;
pub mod history;
pub mod http;
pub mod import;
pub mod interchange;
pub mod model;
pub mod persistence;
pub mod template;
