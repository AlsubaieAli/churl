//! Handler clusters extracted from `app.rs` (M7.11, PR 3–4). Each submodule adds
//! an `impl App` block for one cluster of low-risk handlers. Placed as
//! grandchildren of `app` (children of this `handlers` module) so their
//! `impl App` methods keep full access to `App`'s private fields and methods
//! without any visibility widening — see DECISIONS.md, "Module boundaries"
//! (M7.11). Methods are inherent on `App`, so declaring the submodules is all
//! that's needed; every call site resolves via `self.`.

mod buffers;
mod crud;
mod editing;
mod env_editor;
mod help;
mod load_runner;
mod response;
mod send;
mod sequence;
mod vars;
mod workspace;
