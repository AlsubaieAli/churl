//! Handler clusters extracted from `app.rs`. Each submodule adds
//! an `impl App` block for one cluster of low-risk handlers. Placed as
//! grandchildren of `app` (children of this `handlers` module) so their
//! `impl App` methods keep full access to `App`'s private fields and methods
//! without any visibility widening — see DECISIONS.md, "Module boundaries".
//! Methods are inherent on `App`, so declaring the submodules is all
//! that's needed; every call site resolves via `self.`.

mod buffers;
// `pub(in crate::tui::app)`: `mod.rs`'s `handle_paste` needs the free fn
// `looks_like_curl` (not a method, so it can't reach it via `self.`) to keep
// the expand-trigger predicate identical to the submit-time import check.
pub(in crate::tui::app) mod crud;
mod debug;
mod editing;
mod env_editor;
mod help;
mod load_runner;
mod options;
mod response;
mod send;
mod sequence;
mod vars;
mod workspace;
