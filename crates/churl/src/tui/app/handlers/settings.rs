//! Settings panel handlers (open / key / apply) plus the standalone
//! `<leader>k` insecure toggle. Grandchild module of `app`; the `impl App` here
//! keeps full access to `App`'s private fields without visibility widening.
//!
//! The panel is UI-only: it emits a [`SettingsOutcome`] and the app applies it
//! to the session settings, rebuilds the single client through the install-runtime
//! seam ([`App::rebuild_client`]) where relevant, and refreshes the panel's mirror.
//! The cookie `Arc` survives a rebuild, so toggling cookies off→on keeps the jar.
//!
//! Most knobs apply live (a cheap existing seam — a direct field write, or a
//! client rebuild exactly as the old Options overlay did). `leader_key` is the
//! one exception: no cheap live-reparse seam exists for the keymap, so it
//! updates the working copy only (see [`SettingsOutcome::ApplyLeaderKey`]'s doc)
//! — Save-as-default (M8.5 Wave 3) is what actually persists it.

use super::super::*;
use crate::tui::components::settings::{AdvancedField, mask_proxy};

impl App {
    /// Opens the Settings panel over the current session settings.
    pub(in crate::tui::app) fn open_settings(&mut self) {
        let state = SettingsState::new(self.settings_snapshot());
        // One transition — construct the state INTO the mode (no parallel field).
        self.mode = Mode::Settings(state);
    }

    /// Closes the Settings panel, returning to Normal mode.
    fn close_settings(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Routes a key to the open Settings panel and acts on its outcome. Every
    /// applied change rebuilds the client where relevant and refreshes the
    /// panel's mirror.
    ///
    /// A client rebuild can only fail on a malformed proxy URL; such a failure is
    /// caught, the offending change is rolled back (the prior valid client stays
    /// installed), and the error is shown inline in the panel footer — it must
    /// NEVER propagate out of here, or the run loop's `handle_key(key)?` would tear
    /// down the whole session over a typo.
    pub(in crate::tui::app) fn handle_settings_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Settings(state) = &mut self.mode else {
            return Ok(());
        };
        let outcome = state.handle_key(key);
        match outcome {
            SettingsOutcome::Consumed => {}
            SettingsOutcome::Close => self.close_settings(),
            SettingsOutcome::ApplyProxy(proxy) => {
                match self.with_client_rebuild(|s| s.session_proxy = proxy) {
                    Ok(()) => {
                        let msg = match &self.session_proxy {
                            Some(p) => format!("proxy set: {}", mask_proxy(p)),
                            None => "proxy cleared (using env proxy)".to_owned(),
                        };
                        self.notify(msg);
                    }
                    Err(err) => self.set_settings_message(format!("invalid proxy — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::ToggleInsecure => {
                match self.with_client_rebuild(|s| s.session_insecure = !s.session_insecure) {
                    Ok(()) => self.notify(insecure_message(self.session_insecure)),
                    Err(err) => self.set_settings_message(format!("could not apply — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::ToggleCookies => {
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
                    Err(err) => self.set_settings_message(format!("could not apply — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::DeleteCookie { domain, name } => {
                let removed = self.cookie_jar.delete(&domain, &name);
                if removed {
                    self.persist_cookie_jar();
                }
                self.notify(if removed {
                    format!("deleted cookie {name} ({domain})")
                } else {
                    format!("no cookie {name} ({domain})")
                });
                self.refresh_settings_panel();
            }
            SettingsOutcome::ClearCookies => {
                self.cookie_jar.clear();
                self.persist_cookie_jar();
                self.notify("cookies cleared");
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyAdvanced { field, value } => {
                self.apply_advanced_limit(field, value);
                self.refresh_settings_panel();
            }
            SettingsOutcome::ToggleDebug => {
                self.toggle_debug();
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyRedirect(policy) => {
                self.execute_options.redirect = policy;
                self.notify(format!("redirect policy set to {}", redirect_label(policy)));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyUrlEdit(mode) => {
                self.set_url_edit_mode(mode);
                self.notify(format!("URL edit mode set to {}", url_edit_label(mode)));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplySecretPolicy(policy) => {
                self.set_secret_policy(policy);
                self.notify(format!(
                    "secret policy set to {}",
                    secret_policy_label(policy)
                ));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyLoadCap { field, value } => {
                field.set(&mut self.load_caps, value);
                self.notify(format!("{} set to {value}", field.label()));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyTheme(name) => {
                match Theme::resolve(Some(&name), &self.theme_colors) {
                    Ok(theme) => {
                        self.theme = theme;
                        self.theme_name = name.clone();
                        self.highlight_tx =
                            highlight::spawn(self.tx.clone(), self.theme.is_light());
                        self.notify(format!("theme set to {name}"));
                    }
                    Err(err) => self.set_settings_message(format!("could not apply theme — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyLeaderKey(combo) => {
                self.leader_key = combo.clone();
                self.notify(format!(
                    "leader key set to {combo} (applies on next launch)"
                ));
                self.refresh_settings_panel();
            }
            SettingsOutcome::SaveDefaults => {
                // Wired in M8.5 Wave 3 (Save-as-default): assembles the working
                // copy and calls `churl_core::config::save_defaults`. No key
                // binding emits this outcome yet, so it is unreachable today.
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

    /// Applies a validated Advanced-limit override (M8.3 Wave 4, ported to the
    /// Settings panel's Debug category — and aliased directly by the Request
    /// category's Timeout/MaxBodyBytes rows, which share these same two
    /// fields; see [`crate::tui::components::settings::RequestRow`]'s doc).
    /// Concurrency and total are checked against the SAME `[load]` guardrail
    /// caps `churl_core::load::check_config` enforces at run time — a value
    /// above `max_concurrency`/`max_total` is refused here too, so the panel
    /// can never be used to bypass the guardrail; a value above only the
    /// *warn* threshold is still accepted (this changes a stored default, not
    /// an in-flight run, so there is nothing to confirm y/n against).
    /// Body-cap/timeout have no guardrail cap to check — any positive value
    /// (already enforced by the panel's own edit gate) applies directly.
    fn apply_advanced_limit(&mut self, field: AdvancedField, value: u64) {
        match field {
            AdvancedField::Concurrency | AdvancedField::Total => {
                let mut candidate = churl_core::load::LoadConfig {
                    total: self.advanced_limits.total,
                    concurrency: self.advanced_limits.concurrency,
                    interval: Duration::ZERO,
                };
                let clamped = value.min(usize::MAX as u64) as usize;
                match field {
                    AdvancedField::Concurrency => candidate.concurrency = clamped,
                    AdvancedField::Total => candidate.total = clamped,
                    _ => unreachable!("guarded by the outer match"),
                }
                match churl_core::load::check_config(&candidate, &self.load_caps) {
                    churl_core::load::LoadCheck::Refuse(msg) => {
                        self.set_settings_message(format!("refused — {msg}"));
                    }
                    churl_core::load::LoadCheck::Ok | churl_core::load::LoadCheck::Warn(_) => {
                        self.advanced_limits.concurrency = candidate.concurrency;
                        self.advanced_limits.total = candidate.total;
                        self.notify(format!("advanced {} set to {value}", field.label()));
                    }
                }
            }
            AdvancedField::BodyCapBytes => {
                self.advanced_limits.body_cap_bytes = value;
                self.execute_options.max_body_bytes = value;
                self.notify(format!("advanced body cap set to {value} bytes"));
            }
            AdvancedField::TimeoutSecs => {
                self.advanced_limits.timeout_secs = value;
                self.client_timeout = Duration::from_secs(value);
                match self.rebuild_client() {
                    Ok(()) => self.notify(format!("advanced timeout set to {value}s")),
                    Err(err) => self.set_settings_message(format!("could not apply — {err}")),
                }
            }
        }
    }

    /// Sets an inline message in the open Settings panel footer (a no-op when
    /// the panel is not open). Used to surface a rejected change without a crash.
    fn set_settings_message(&mut self, msg: impl Into<String>) {
        if let Mode::Settings(state) = &mut self.mode {
            state.message = Some(msg.into());
        }
    }

    /// A snapshot of every session-scoped value the panel manages, taken from
    /// the app's current state — used both to open the panel and (via
    /// [`Self::refresh_settings_panel`]) to refresh its mirror after a change.
    fn settings_snapshot(&self) -> crate::tui::components::settings::SettingsSnapshot {
        crate::tui::components::settings::SettingsSnapshot {
            redirect: self.execute_options.redirect,
            url_edit: self.url_edit_mode,
            secret_policy: self.secret_policy,
            proxy: self.session_proxy.clone(),
            insecure: self.session_insecure,
            cookies_enabled: self.cookies_enabled,
            cookies: self.cookie_jar.list(),
            load_caps: self.load_caps,
            theme_name: self.theme_name.clone(),
            leader_key: self.leader_key.clone(),
            debug_enabled: self.debug_enabled,
            advanced: self.advanced_limits,
        }
    }

    /// Refreshes the open Settings panel's mirror of the session settings after
    /// an applied change. A no-op when the panel is not open. Preserves any
    /// inline message already set (so a rejection note survives the refresh).
    fn refresh_settings_panel(&mut self) {
        let snapshot = self.settings_snapshot();
        if let Mode::Settings(state) = &mut self.mode {
            state.refresh(snapshot);
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
        self.refresh_settings_panel();
    }

    /// Toggles the SELECTED endpoint's durable insecure-TLS opt-in (`<leader>K`)
    /// and persists it onto the endpoint file — distinct from the session-wide
    /// `<leader>k`. The flag rides on the request, so a send of this endpoint goes
    /// out with cert verification off while sibling endpoints keep verifying. Loud
    /// on the way on, since it disables verification for every future send. A save
    /// failure rolls the in-memory flip back so state matches disk.
    pub(in crate::tui::app) fn toggle_endpoint_insecure(&mut self) {
        let Some(sel) = self.selected_mut() else {
            self.message = Some(Message::new("no endpoint selected"));
            return;
        };
        sel.endpoint.request.insecure = !sel.endpoint.request.insecure;
        let now_on = sel.endpoint.request.insecure;
        let path = sel.file.clone();
        let endpoint = sel.endpoint.clone();
        match persistence::save_endpoint(&path, &endpoint) {
            Ok(()) => {
                // Keep the saved snapshot in sync so the change doesn't read as
                // unsaved, and refresh the explorer's cached copy.
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.loaded_snapshot = endpoint.clone();
                }
                self.refresh_explorer_endpoint(&path, endpoint);
                self.notify(endpoint_insecure_message(now_on));
            }
            Err(err) => {
                if let Some(sel) = self.selected_mut() {
                    sel.endpoint.request.insecure = !now_on;
                }
                self.message = Some(Message::new(format!("could not save endpoint: {err}")));
            }
        }
    }

    /// Whether TLS verification is currently OFF for the selected endpoint — the
    /// **effective** insecure (`session_insecure || selected.request.insecure`) —
    /// driving the loud statusline indicator. Reflects the durable per-endpoint
    /// opt-in as well as the session-wide override. `pub(crate)` so the render
    /// layer can read it.
    pub(crate) fn insecure_active(&self) -> bool {
        self.session_insecure || self.selected().is_some_and(|s| s.endpoint.request.insecure)
    }
}

/// The status message for a session-wide insecure-TLS toggle.
fn insecure_message(insecure: bool) -> String {
    if insecure {
        "⚠ TLS verification OFF — certificates not checked".to_owned()
    } else {
        "TLS verification on".to_owned()
    }
}

/// The status message for a per-endpoint insecure-TLS toggle (persisted).
fn endpoint_insecure_message(insecure: bool) -> String {
    if insecure {
        "⚠ TLS verification OFF for this endpoint — saved".to_owned()
    } else {
        "TLS verification on for this endpoint — saved".to_owned()
    }
}

fn redirect_label(policy: churl_core::config::RedirectPolicy) -> &'static str {
    use churl_core::config::RedirectPolicy;
    match policy {
        RedirectPolicy::Strip => "strip",
        RedirectPolicy::Strict => "strict",
        RedirectPolicy::FollowAll => "follow-all",
    }
}

fn url_edit_label(mode: churl_core::config::UrlEditMode) -> &'static str {
    use churl_core::config::UrlEditMode;
    match mode {
        UrlEditMode::Inline => "inline",
        UrlEditMode::Popup => "popup",
    }
}

fn secret_policy_label(policy: churl_core::secrets::SecretPolicy) -> &'static str {
    use churl_core::secrets::SecretPolicy;
    match policy {
        SecretPolicy::Strict => "strict",
        SecretPolicy::Warn => "warn",
    }
}
