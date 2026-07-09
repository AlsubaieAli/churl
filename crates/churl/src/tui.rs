//! Terminal lifecycle (init/restore) and the TUI entry point. All state and
//! rendering live in [`app`], [`components`], and [`events`].

pub mod app;
pub mod clipboard;
pub mod components;
pub mod events;
pub mod highlight;
pub mod theme;

use std::collections::BTreeMap;

use color_eyre::Result;
use ratatui::DefaultTerminal;

use app::App;
use events::KeyMap;
use theme::Theme;

/// Initialise the terminal (raw mode + alternate screen, via `ratatui::init`).
pub fn init() -> DefaultTerminal {
    ratatui::init()
}

/// Restore the terminal to its original state. Safe to call multiple times.
pub fn restore() {
    ratatui::restore();
}

/// Builds the [`App`] (workspace from cwd, keymap + theme from global config,
/// CLI `--var`/`--profile` overrides) and runs it until quit. Config, theme, and
/// unknown-profile errors all surface before the alternate screen is entered.
pub async fn run(cli_vars: BTreeMap<String, String>, profile: Option<String>) -> Result<()> {
    let config = churl_core::config::load_global_config()?;
    let mut keymap = KeyMap::with_all_overrides(&config.keys, &config.key_overlays)?;
    if let Some(leader) = config.leader_key.as_deref() {
        keymap.set_leader(leader)?;
    }
    // Load-time conflict/shadow validation (M7.10). Non-blocking: warnings go to
    // stderr *before* raw mode (visible in the launching shell), then to a
    // first-frame statusline toast, and the full list lives in `churl keymaps`.
    let keymap_warnings = keymap.validate(&config.keys, &config.key_overlays);
    for warning in &keymap_warnings {
        eprintln!("churl: keymap warning: {warning}");
    }
    let url_edit = config.url_edit()?;
    let theme = Theme::resolve(config.theme.as_deref(), &config.theme_colors)?;
    let cwd = std::env::current_dir()?;
    let workspace = app::open_workspace(&cwd)?;
    let mut app = App::with_config(workspace, keymap, theme, cli_vars, profile)?;
    app.set_url_edit_mode(url_edit);
    app.set_keymap_warnings(keymap_warnings);
    app.install_runtime(&config)?;

    let mut terminal = init();
    let result = app.run(&mut terminal).await;
    restore();
    result
}
