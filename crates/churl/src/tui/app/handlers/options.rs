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
    ///
    /// A client rebuild can only fail on a malformed proxy URL; such a failure is
    /// caught, the offending change is rolled back (the prior valid client stays
    /// installed), and the error is shown inline in the overlay footer — it must
    /// NEVER propagate out of here, or the run loop's `handle_key(key)?` would tear
    /// down the whole session over a typo.
    pub(in crate::tui::app) fn handle_options_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Options(state) = &mut self.mode else {
            return Ok(());
        };
        let outcome = state.handle_key(key);
        match outcome {
            OptionsOutcome::Consumed => {}
            OptionsOutcome::Close => self.close_options(),
            OptionsOutcome::ApplyProxy(proxy) => {
                match self.with_client_rebuild(|s| s.session_proxy = proxy) {
                    Ok(()) => {
                        let msg = match &self.session_proxy {
                            Some(p) => format!("proxy set: {}", mask_proxy(p)),
                            None => "proxy cleared (using env proxy)".to_owned(),
                        };
                        self.notify(msg);
                    }
                    Err(err) => self.set_options_message(format!("invalid proxy — {err}")),
                }
                self.refresh_options_overlay();
            }
            OptionsOutcome::ToggleInsecure => {
                match self.with_client_rebuild(|s| s.session_insecure = !s.session_insecure) {
                    Ok(()) => self.notify(insecure_message(self.session_insecure)),
                    Err(err) => self.set_options_message(format!("could not apply — {err}")),
                }
                self.refresh_options_overlay();
            }
            OptionsOutcome::ToggleCookies => {
                match self.with_client_rebuild(|s| s.cookies_enabled = !s.cookies_enabled) {
                    Ok(()) => {
                        // Persist on the way off, so the in-RAM jar's persistent
                        // cookies are captured before the client stops receiving new
                        // ones. (Only after a successful rebuild — a rolled-back
                        // toggle must not trigger a spurious save.)
                        if !self.cookies_enabled {
                            self.persist_cookie_jar();
                        }
                        self.notify(if self.cookies_enabled {
                            "cookie jar enabled"
                        } else {
                            "cookie jar disabled"
                        });
                    }
                    Err(err) => self.set_options_message(format!("could not apply — {err}")),
                }
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

    /// Applies a session-setting `mutate`, then rebuilds the single client. A
    /// rebuild failure (only a malformed proxy can cause one) rolls back ALL three
    /// session controls to their prior — necessarily valid — values and returns the
    /// error string for inline display. `rebuild_client` reassigns `self.client`
    /// only on success (its `?` returns before the assignment), so on failure the
    /// previous valid client stays installed; there is nothing to rebuild again.
    /// This is the single guard that keeps a bad setting from killing the session.
    fn with_client_rebuild(
        &mut self,
        mutate: impl FnOnce(&mut Self),
    ) -> std::result::Result<(), String> {
        let prev = (
            self.session_proxy.clone(),
            self.session_insecure,
            self.cookies_enabled,
        );
        mutate(self);
        if let Err(err) = self.rebuild_client() {
            (
                self.session_proxy,
                self.session_insecure,
                self.cookies_enabled,
            ) = prev;
            return Err(err.to_string());
        }
        Ok(())
    }

    /// Sets an inline message in the open Options overlay footer (a no-op when the
    /// overlay is not open). Used to surface a rejected change without a crash.
    fn set_options_message(&mut self, msg: impl Into<String>) {
        if let Mode::Options(state) = &mut self.mode {
            state.message = Some(msg.into());
        }
    }

    /// Refreshes the open Options overlay's mirror of the session settings after
    /// an applied change. A no-op when the overlay is not open. Preserves any
    /// inline message already set (so a rejection note survives the refresh).
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
    /// change takes effect immediately and surfaces a loud message. Never fails the
    /// caller: a rebuild error (only a malformed proxy already in effect could
    /// cause one) rolls back and notifies rather than propagating a session-killing
    /// error up through `dispatch`/`handle_key`.
    pub(in crate::tui::app) fn toggle_insecure(&mut self) {
        match self.with_client_rebuild(|s| s.session_insecure = !s.session_insecure) {
            Ok(()) => self.notify(insecure_message(self.session_insecure)),
            Err(err) => self.notify(format!("could not toggle TLS — {err}")),
        }
        self.refresh_options_overlay();
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
