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
pub async fn run(
    cli_vars: BTreeMap<String, String>,
    profile: Option<String>,
    proxy: Option<String>,
    insecure: bool,
) -> Result<()> {
    let config = churl_core::config::load_global_config()?;
    let mut keymap = KeyMap::with_all_overrides(&config.keys, &config.key_overlays)?;
    if let Some(leader) = config.leader_key.as_deref() {
        keymap.set_leader(leader)?;
    }
    // Load-time conflict/shadow validation. Non-blocking: warnings go to
    // stderr *before* raw mode (visible in the launching shell), then to a
    // first-frame statusline toast, and the full list lives in `churl keymaps`.
    let keymap_warnings = keymap.validate(&config.keys, &config.key_overlays);
    for warning in &keymap_warnings {
        eprintln!("churl: keymap warning: {warning}");
    }
    let url_edit = config.url_edit()?;
    let secret_policy = config.secret_policy()?;
    let theme = Theme::resolve(config.theme.as_deref(), &config.theme_colors)?;
    let cwd = std::env::current_dir()?;
    // Advisory version pin: warn (never refuse) if `.churl-version` at the
    // workspace root names a version the running binary does not satisfy. To
    // stderr before raw mode, so it is visible in the launching shell.
    warn_on_version_mismatch(&cwd);
    let workspace = app::open_workspace(&cwd)?;
    let mut app = App::with_config(workspace, keymap, theme, cli_vars, profile)?;
    app.set_url_edit_mode(url_edit);
    app.set_secret_policy(secret_policy);
    app.set_keymap_warnings(keymap_warnings);
    app.install_runtime(&config, proxy, insecure)?;

    let mut terminal = init();
    let result = app.run(&mut terminal).await;
    restore();
    // Surface a failed FINAL on-quit cookie flush now that the terminal is back —
    // it could not be shown on the (torn-down) statusline. Non-fatal.
    if let Some(err) = app.take_cookie_exit_error() {
        eprintln!("churl: failed to persist cookies on exit: {err}");
    }
    result
}

/// Surfaces the advisory version pin at workspace load: if `.churl-version` at
/// `workspace_root` names a version the running binary does not satisfy, print a
/// warning to stderr and continue. Absent or matching ⇒ nothing.
pub(crate) fn warn_on_version_mismatch(workspace_root: &std::path::Path) {
    if let Some(warning) = pin_warning(workspace_root, env!("CARGO_PKG_VERSION")) {
        eprintln!("{warning}");
    }
}

/// Pure half of [`warn_on_version_mismatch`]: returns the warning line when the
/// workspace pins a version `running` does not satisfy, else `None`. Absent,
/// matching, malformed, or unreadable pins all yield `None` (an advisory hint
/// must never block launch or panic). Split out so it is unit-testable without
/// capturing stderr. Discovery/parse/compare live in [`churl_core::pin`].
fn pin_warning(workspace_root: &std::path::Path, running: &str) -> Option<String> {
    use churl_core::pin::{PinCheck, check_pin, discover_pin, read_pin};

    let path = discover_pin(workspace_root)?;
    let pinned = read_pin(&path).ok().flatten()?;
    match check_pin(&pinned, running) {
        PinCheck::Mismatch { pinned, running } => Some(format!(
            "churl: warning: workspace pins version {pinned}, but this is churl {running} \
             (running anyway; see `.churl-version`)"
        )),
        PinCheck::Satisfied => None,
    }
}

#[cfg(test)]
mod tests {
    use super::pin_warning;
    use churl_core::pin::PIN_FILENAME;

    #[test]
    fn pin_warning_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(pin_warning(dir.path(), "0.2.0").is_none());
    }

    #[test]
    fn pin_warning_matching_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PIN_FILENAME), "0.2.0\n").unwrap();
        assert!(pin_warning(dir.path(), "0.2.0").is_none());
        // A `v`-prefixed pin still matches (no spurious warn).
        std::fs::write(dir.path().join(PIN_FILENAME), "v0.2.0\n").unwrap();
        assert!(pin_warning(dir.path(), "0.2.0").is_none());
    }

    #[test]
    fn pin_warning_mismatch_warns_naming_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PIN_FILENAME), "9.9.9\n").unwrap();
        let warning = pin_warning(dir.path(), "0.2.0").expect("should warn");
        assert!(warning.contains("9.9.9"), "{warning}");
        assert!(warning.contains("0.2.0"), "{warning}");
    }

    #[test]
    fn pin_warning_malformed_is_none_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PIN_FILENAME), "   \n\t\n").unwrap();
        assert!(pin_warning(dir.path(), "0.2.0").is_none());
    }
}
