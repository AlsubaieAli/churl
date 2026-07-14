//! Options overlay handlers (open / key / apply) plus the standalone
//! `<leader>k` insecure toggle. Grandchild module of `app`; the `impl App` here
//! keeps full access to `App`'s private fields without visibility widening.
//!
//! The overlay is UI-only: it emits an [`OptionsOutcome`] and the app applies it
//! to the session settings, rebuilds the single client through the install-runtime
//! seam ([`App::rebuild_client`]), and refreshes the overlay's mirror. The cookie
//! `Arc` survives the rebuild, so toggling cookies off→on keeps the jar.

use super::super::*;
use crate::tui::components::options::mask_proxy;

impl App {
    /// Opens the session Options overlay over the current settings.
    pub(in crate::tui::app) fn open_options(&mut self) {
        let cookies = self.cookie_jar.list();
        let state = OptionsState::new(
            self.session_proxy.clone(),
            self.session_insecure,
            self.cookies_enabled,
            cookies,
        );
        // One transition — construct the state INTO the mode (no parallel field).
        self.mode = Mode::Options(state);
    }

    /// Closes the Options overlay, returning to Normal mode.
    fn close_options(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Routes a key to the open Options overlay and acts on its outcome. Every
    /// applied change rebuilds the client and refreshes the overlay's mirror.
    pub(in crate::tui::app) fn handle_options_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Options(state) = &mut self.mode else {
            return Ok(());
        };
        let outcome = state.handle_key(key);
        match outcome {
            OptionsOutcome::Consumed => {}
            OptionsOutcome::Close => self.close_options(),
            OptionsOutcome::ApplyProxy(proxy) => {
                self.session_proxy = proxy;
                self.rebuild_client()?;
                let msg = match &self.session_proxy {
                    Some(p) => format!("proxy set: {}", mask_proxy(p)),
                    None => "proxy cleared (using env proxy)".to_owned(),
                };
                self.notify(msg);
                self.refresh_options_overlay();
            }
            OptionsOutcome::ToggleInsecure => {
                self.session_insecure = !self.session_insecure;
                self.rebuild_client()?;
                self.notify(insecure_message(self.session_insecure));
                self.refresh_options_overlay();
            }
            OptionsOutcome::ToggleCookies => {
                self.cookies_enabled = !self.cookies_enabled;
                // Persist on the way off, so the in-RAM jar's persistent cookies
                // are captured before it stops receiving new ones.
                if !self.cookies_enabled {
                    self.persist_cookie_jar();
                }
                self.rebuild_client()?;
                self.notify(if self.cookies_enabled {
                    "cookie jar enabled"
                } else {
                    "cookie jar disabled"
                });
                self.refresh_options_overlay();
            }
            OptionsOutcome::DeleteCookie { domain, name } => {
                let removed = self.cookie_jar.delete(&domain, &name);
                if removed {
                    self.persist_cookie_jar();
                }
                self.notify(if removed {
                    format!("deleted cookie {name} ({domain})")
                } else {
                    format!("no cookie {name} ({domain})")
                });
                self.refresh_options_overlay();
            }
            OptionsOutcome::ClearCookies => {
                self.cookie_jar.clear();
                self.persist_cookie_jar();
                self.notify("cookies cleared");
                self.refresh_options_overlay();
            }
        }
        Ok(())
    }

    /// Refreshes the open Options overlay's mirror of the session settings after
    /// an applied change. A no-op when the overlay is not open.
    fn refresh_options_overlay(&mut self) {
        let proxy = self.session_proxy.clone();
        let insecure = self.session_insecure;
        let enabled = self.cookies_enabled;
        let cookies = self.cookie_jar.list();
        if let Mode::Options(state) = &mut self.mode {
            state.refresh(proxy, insecure, enabled, cookies);
        }
    }

    /// Toggles insecure-TLS from anywhere (`<leader>k`). Rebuilds the client so the
    /// change takes effect immediately, surfaces a loud message, and refreshes the
    /// Options overlay if it happens to be open.
    pub(in crate::tui::app) fn toggle_insecure(&mut self) -> Result<()> {
        self.session_insecure = !self.session_insecure;
        self.rebuild_client()?;
        self.notify(insecure_message(self.session_insecure));
        self.refresh_options_overlay();
        Ok(())
    }

    /// Whether TLS verification is currently OFF (drives the loud statusline
    /// indicator). `pub(crate)` so the render layer can read it.
    pub(crate) fn insecure_active(&self) -> bool {
        self.session_insecure
    }
}

/// The status message for an insecure-TLS toggle.
fn insecure_message(insecure: bool) -> String {
    if insecure {
        "⚠ TLS verification OFF — certificates not checked".to_owned()
    } else {
        "TLS verification on".to_owned()
    }
}
