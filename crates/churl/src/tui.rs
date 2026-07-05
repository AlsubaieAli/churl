//! Terminal lifecycle (init/restore) and the TUI entry point. All state and
//! rendering live in [`app`], [`components`], and [`events`].

pub mod app;
pub mod components;
pub mod events;
pub mod highlight;

use color_eyre::Result;
use ratatui::DefaultTerminal;

use app::App;
use events::KeyMap;

/// Initialise the terminal (raw mode + alternate screen, via `ratatui::init`).
pub fn init() -> DefaultTerminal {
    ratatui::init()
}

/// Restore the terminal to its original state. Safe to call multiple times.
pub fn restore() {
    ratatui::restore();
}

/// Builds the [`App`] (workspace from cwd, keymap from global config) and runs
/// it until quit. Config errors surface before the alternate screen is entered.
pub async fn run() -> Result<()> {
    let config = churl_core::config::load_global_config()?;
    let keymap = KeyMap::with_overrides(&config.keys)?;
    let cwd = std::env::current_dir()?;
    let workspace = app::open_workspace(&cwd)?;
    let mut app = App::new(workspace, keymap)?;
    app.install_runtime(&config)?;

    let mut terminal = init();
    let result = app.run(&mut terminal).await;
    restore();
    result
}
