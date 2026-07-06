//! Ratatui component tree: one module per pane/overlay. All render functions
//! are pure (state in, widgets out) so snapshot tests can drive them through a
//! `TestBackend` without a tokio runtime.

pub mod explorer;
pub mod jump;
pub mod line_editor;
pub mod method_menu;
pub mod palette;
pub mod picker;
pub mod prompt;
pub mod request;
pub mod request_tabs;
pub mod response;
pub mod search;
pub mod statusline;
pub mod urlbar;
