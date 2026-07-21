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
//! one exception that does NOT apply on this keystroke: a cheap live-reparse
//! seam DOES exist (`KeyMap::set_leader`, M8.5.3), but a panel edit only
//! updates the working copy (see [`SettingsOutcome::ApplyLeaderKey`]'s doc) —
//! the explicit apply gate is `<leader>r` (`App::reload_workspace`), which
//! calls that seam. Save-as-default (M8.5 Wave 3) is what persists it to disk.

use super::super::*;
use crate::tui::components::settings::{AdvancedField, SettingKey, mask_proxy};
use churl_core::cookies::CookieSpec;

impl App {
    /// Opens the Settings panel over the current session settings. Seeds the
    /// net-change **baseline** ([`Self::settings_baseline`]) from the
    /// session's effective values ONLY on the first-ever open this session
    /// (M8.5.3 fix) — a later reopen leaves the baseline exactly as it was
    /// (the session-start values, or the just-saved values if a Save
    /// happened since). Every panel edit is judged net-changed-or-not
    /// against this baseline, so a knob merely interacted with (a toggle
    /// round-trip, an Enter-commit that changed nothing) never ends up
    /// persisted — and, critically, a knob genuinely changed but never saved
    /// stays touched across a close/reopen instead of being silently
    /// re-baselined at its own dirty live value (which used to make a later
    /// net-zero-looking adjust erase it and Save write nothing; see
    /// [`Self::settings_baseline_established`]'s doc).
    pub(in crate::tui::app) fn open_settings(&mut self) {
        if !self.settings_baseline_established {
            self.settings_baseline = self.settings_working_copy();
            self.settings_baseline_established = true;
        }
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
    ///
    /// FIX 1 (owner decision) + net-change guard: every outcome arm that
    /// actually applies a change calls [`Self::mark_setting`] with whether the
    /// new value **net-differs from the panel-open baseline**
    /// ([`Self::settings_baseline`], captured in [`Self::open_settings`]).
    /// `mark_setting` INSERTS on a real net change and REMOVES on a net-zero
    /// one (a toggle back to origin, or an Enter-commit that didn't change the
    /// value) — so a knob the user merely *interacted with* without changing
    /// never persists its CLI/`-k`/workspace-forced value. Marking happens
    /// HERE rather than inside a shared helper (`toggle_debug`,
    /// `toggle_insecure`) that a non-panel keybinding also calls, so a global
    /// shortcut is never mistaken for a panel edit.
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
                        // Compares REAL proxy values (not the masked display),
                        // so re-committing the seeded forced proxy unchanged is
                        // correctly net-zero.
                        let differs = self.session_proxy != self.settings_baseline.proxy;
                        self.mark_setting(SettingKey::Proxy, differs);
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
                    Ok(()) => {
                        let differs = self.session_insecure != self.settings_baseline.insecure;
                        self.mark_setting(SettingKey::Insecure, differs);
                        self.notify(insecure_message(self.session_insecure));
                    }
                    Err(err) => self.set_settings_message(format!("could not apply — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::ToggleCookies => {
                match self.with_client_rebuild(|s| s.cookies_enabled = !s.cookies_enabled) {
                    Ok(()) => {
                        let differs = self.cookies_enabled != self.settings_baseline.cookies;
                        self.mark_setting(SettingKey::Cookies, differs);
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
                // Jar CONTENTS are not a settings-panel-managed knob (they live
                // in `state.sqlite`, not `config.toml`) — no `SettingKey` to mark.
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
            SettingsOutcome::UpsertCookie {
                previous,
                domain,
                name,
                value,
                path,
                secure,
                same_site,
            } => {
                // Jar CONTENTS are not a settings-panel-managed knob (mirrors
                // DeleteCookie/ClearCookies above) — no `SettingKey` to mark.
                let is_edit = previous.is_some();
                let spec = CookieSpec {
                    domain: domain.clone(),
                    name: name.clone(),
                    value,
                    path: path.clone(),
                    secure,
                    same_site,
                };
                match self.cookie_jar.upsert(spec) {
                    Ok(()) => {
                        // Upsert FIRST, then remove the old coordinate — only
                        // now that the new one is safely in. When an edit
                        // changed the key (domain/name/path), drop the OLD
                        // entry with the PATH-PRECISE `delete_exact`, never the
                        // domain+name-scoped `delete` (which would wipe a
                        // same-name sibling at another path). Because the new
                        // key differs from the old by definition of "changed,"
                        // deleting the exact old coord can't touch the row this
                        // upsert just added, and a failed upsert (below) skips
                        // the delete entirely, so the original always survives.
                        if let Some((old_domain, old_name, old_path)) = &previous
                            && (*old_domain != domain || *old_name != name || *old_path != path)
                        {
                            self.cookie_jar.delete_exact(old_domain, old_path, old_name);
                        }
                        self.persist_cookie_jar();
                        let verb = if is_edit { "updated" } else { "added" };
                        // Never echo the value — it's credential-shaped, same
                        // stance as `DeleteCookie`'s notification.
                        self.notify(format!("{verb} cookie {name} ({domain})"));
                    }
                    Err(err) => self.set_settings_message(format!("could not save cookie — {err}")),
                }
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyAdvanced { field, value } => {
                // Marks the right `SettingKey` internally, on exactly the
                // branches that actually change state, against the baseline.
                self.apply_advanced_limit(field, value);
                self.refresh_settings_panel();
            }
            SettingsOutcome::ToggleDebug => {
                // `toggle_debug` is SHARED with the global `<leader>D` shortcut
                // (see its doc) — touched is marked HERE, not inside it, so
                // `<leader>D` outside the panel never marks it.
                self.toggle_debug();
                let differs = self.debug_enabled != self.settings_baseline.debug;
                self.mark_setting(SettingKey::Debug, differs);
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyRedirect(policy) => {
                self.execute_options.redirect = policy;
                let differs = self.execute_options.redirect != self.settings_baseline.redirect;
                self.mark_setting(SettingKey::Redirect, differs);
                self.notify(format!("redirect policy set to {}", redirect_label(policy)));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyUrlEdit(mode) => {
                self.set_url_edit_mode(mode);
                let differs = self.url_edit_mode != self.settings_baseline.url_edit;
                self.mark_setting(SettingKey::UrlEdit, differs);
                self.notify(format!("URL edit mode set to {}", url_edit_label(mode)));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplySecretPolicy(policy) => {
                self.set_secret_policy(policy);
                let differs = self.secret_policy != self.settings_baseline.secret_policy;
                self.mark_setting(SettingKey::SecretPolicy, differs);
                self.notify(format!(
                    "secret policy set to {}",
                    secret_policy_label(policy)
                ));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyLoadCap { field, value } => {
                field.set(&mut self.load_caps, value);
                let differs =
                    field.get(&self.load_caps) != field.get(&self.settings_baseline.load_caps);
                self.mark_setting(field.setting_key(), differs);
                self.notify(format!("{} set to {value}", field.label()));
                self.refresh_settings_panel();
            }
            SettingsOutcome::ApplyTheme(name) => {
                match Theme::resolve(Some(&name), &self.theme_colors) {
                    Ok(theme) => {
                        self.theme = theme;
                        self.theme_name = name.clone();
                        let differs = self.theme_name != self.settings_baseline.theme;
                        self.mark_setting(SettingKey::Theme, differs);
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
                // Canonical combo compare, not raw string equality — capturing
                // the already-bound key (e.g. Space → crokey's `"Space"` vs the
                // stored default `"space"`) is a net-zero interaction and must
                // not mark the knob touched (no dirty dot, no net-zero rewrite).
                let differs = !crate::tui::components::settings::leader_key_eq(
                    &self.leader_key,
                    &self.settings_baseline.leader_key,
                );
                self.mark_setting(SettingKey::LeaderKey, differs);
                self.notify(format!(
                    "leader key set to {combo} — press <leader>r (or restart) to apply"
                ));
                self.refresh_settings_panel();
            }
            SettingsOutcome::SaveDefaults => {
                self.save_settings_defaults();
                self.refresh_settings_panel();
            }
        }
        Ok(())
    }

    /// Records the net-change state of one panel knob against the panel-open
    /// baseline: INSERT into [`Self::settings_touched`] when the new value
    /// genuinely differs from what it was when the panel opened, REMOVE it
    /// when the value is back at (or never left) that baseline. This is what
    /// makes the touched-set mean "net-changed in the panel", not merely
    /// "interacted with" — so a toggle-back-to-origin, or an Enter-commit that
    /// didn't change anything, leaves nothing to persist.
    fn mark_setting(&mut self, key: SettingKey, differs: bool) {
        if differs {
            self.settings_touched.insert(key);
        } else {
            self.settings_touched.remove(&key);
        }
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
    ///
    /// This fn is reached ONLY from the panel's `ApplyAdvanced` outcome (see
    /// its one call site in [`Self::handle_settings_key`]), so it is safe —
    /// and the clearest place — to net-change-mark
    /// [`crate::tui::components::settings::SettingKey`] here, on exactly the
    /// branches that actually change state (a `Refuse`d concurrency/total edit
    /// changes nothing, so it must not touch the set). Marking is against the
    /// panel-open baseline via [`Self::mark_setting`], so re-entering a field's
    /// existing value is net-zero.
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
                        let differs = field.get(&self.advanced_limits)
                            != field.get(&self.settings_baseline.advanced);
                        self.mark_setting(field.setting_key(), differs);
                        self.notify(format!("advanced {} set to {value}", field.label()));
                    }
                }
            }
            AdvancedField::BodyCapBytes => {
                self.advanced_limits.body_cap_bytes = value;
                self.execute_options.max_body_bytes = value;
                let differs =
                    field.get(&self.advanced_limits) != field.get(&self.settings_baseline.advanced);
                self.mark_setting(field.setting_key(), differs);
                self.notify(format!("advanced body cap set to {value} bytes"));
            }
            AdvancedField::TimeoutSecs => {
                self.advanced_limits.timeout_secs = value;
                self.client_timeout = Duration::from_secs(value);
                // `advanced_limits.timeout_secs` is NOT rolled back on a
                // rebuild failure, so the working copy really did change — mark
                // by net-change against the baseline either way.
                let differs =
                    field.get(&self.advanced_limits) != field.get(&self.settings_baseline.advanced);
                self.mark_setting(field.setting_key(), differs);
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

    /// Every managed knob's current session value in the FULL resolved shape,
    /// used as the net-change baseline ([`Self::settings_baseline`], captured
    /// at [`Self::open_settings`] and re-captured after a save). This is the
    /// panel's working copy exactly as displayed — the sparse write-path shape
    /// is [`Self::settings_edits`].
    fn settings_working_copy(&self) -> churl_core::config::ResolvedSettings {
        churl_core::config::ResolvedSettings {
            theme: self.theme_name.clone(),
            leader_key: self.leader_key.clone(),
            timeout_secs: self.advanced_limits.timeout_secs,
            max_body_bytes: self.advanced_limits.body_cap_bytes,
            url_edit: self.url_edit_mode,
            secret_policy: self.secret_policy,
            redirect: self.execute_options.redirect,
            proxy: self.session_proxy.clone(),
            insecure: self.session_insecure,
            cookies: self.cookies_enabled,
            debug: self.debug_enabled,
            load_caps: self.load_caps,
            advanced: self.advanced_limits,
        }
    }

    /// The current session's working copy, assembled into the SPARSE shape
    /// [`churl_core::config::save_defaults`] persists (FIX 1: only a knob in
    /// [`Self::settings_touched`] gets a `Some` here — everything else is
    /// `None`, so the writer never even reads its on-disk key, whatever the
    /// live session value is). Request's `timeout_secs`/`max_body_bytes` and
    /// Debug's Advanced `timeout`/`body cap` alias the SAME live field
    /// (`self.advanced_limits`, see
    /// [`crate::tui::components::settings::RequestRow`]'s doc) — and the SAME
    /// [`crate::tui::components::settings::SettingKey`] — so editing either
    /// surface touches the one field this reads.
    fn settings_edits(&self) -> churl_core::config::SettingsDefaults {
        let t = &self.settings_touched;
        churl_core::config::SettingsDefaults {
            theme: t
                .contains(&SettingKey::Theme)
                .then(|| self.theme_name.clone()),
            leader_key: t
                .contains(&SettingKey::LeaderKey)
                .then(|| self.leader_key.clone()),
            timeout_secs: t
                .contains(&SettingKey::Timeout)
                .then_some(self.advanced_limits.timeout_secs),
            max_body_bytes: t
                .contains(&SettingKey::MaxBodyBytes)
                .then_some(self.advanced_limits.body_cap_bytes),
            url_edit: t
                .contains(&SettingKey::UrlEdit)
                .then_some(self.url_edit_mode),
            secret_policy: t
                .contains(&SettingKey::SecretPolicy)
                .then_some(self.secret_policy),
            redirect: t
                .contains(&SettingKey::Redirect)
                .then_some(self.execute_options.redirect),
            proxy: t
                .contains(&SettingKey::Proxy)
                .then(|| self.session_proxy.clone()),
            insecure: t
                .contains(&SettingKey::Insecure)
                .then_some(self.session_insecure),
            cookies: t
                .contains(&SettingKey::Cookies)
                .then_some(self.cookies_enabled),
            debug: t.contains(&SettingKey::Debug).then_some(self.debug_enabled),
            load_warn_total: t
                .contains(&SettingKey::LoadWarnTotal)
                .then_some(self.load_caps.warn_total),
            load_warn_concurrency: t
                .contains(&SettingKey::LoadWarnConcurrency)
                .then_some(self.load_caps.warn_concurrency),
            load_max_total: t
                .contains(&SettingKey::LoadMaxTotal)
                .then_some(self.load_caps.max_total),
            load_max_concurrency: t
                .contains(&SettingKey::LoadMaxConcurrency)
                .then_some(self.load_caps.max_concurrency),
            advanced_concurrency: t
                .contains(&SettingKey::AdvancedConcurrency)
                .then_some(self.advanced_limits.concurrency),
            advanced_total: t
                .contains(&SettingKey::AdvancedTotal)
                .then_some(self.advanced_limits.total),
        }
    }

    /// Persists the current working copy to `config.toml` (M8.5 Wave 3,
    /// `s` inside the Settings panel) — ONLY the knobs actually edited in the
    /// panel this session (FIX 1: [`Self::settings_edits`] is sparse). Never
    /// silent: a confirm toast names the written file on success; a write
    /// failure surfaces as a loud inline error in the panel's own message
    /// slot (visible regardless of which level/category is open) rather than
    /// being swallowed.
    ///
    /// A panel-typed credentialed proxy does NOT fail the save (FIX 4): the
    /// writer skips just that key and reports it via
    /// [`churl_core::config::SaveOutcome::proxy_skipped`], surfaced here as a
    /// toast rather than an error — every OTHER touched knob still persisted.
    /// The touched-set clears on success EXCEPT `Proxy` when it was skipped:
    /// that knob genuinely was not saved, so its dirty dot must stay lit
    /// (clearing it would silently claim persistence that didn't happen) —
    /// the next `s` retries it.
    ///
    /// On success the net-change **baseline** is re-captured to the just-saved
    /// working copy, so a later edit compares against the freshly-persisted
    /// state (e.g. saving timeout 30→60 then setting it back to 60 is
    /// correctly net-zero, while back to 30 is a real change again). In the
    /// proxy-skip case the proxy baseline is left as-is — the proxy was NOT
    /// persisted, so its "no net change" origin is unchanged, keeping the
    /// still-unsaved credentialed proxy net-changed for the retry.
    fn save_settings_defaults(&mut self) {
        let path = match churl_core::config::resolve_settings_path() {
            Ok(path) => path,
            Err(err) => {
                self.set_settings_message(format!("save failed — {err}"));
                return;
            }
        };
        let edits = self.settings_edits();
        match churl_core::config::save_defaults(&edits, &path) {
            Ok(outcome) if outcome.proxy_skipped => {
                self.settings_touched.retain(|k| *k == SettingKey::Proxy);
                // Re-baseline everything that WAS persisted; keep the proxy
                // baseline so the still-unsaved credentialed proxy stays a net
                // change (and its dirty dot stays lit) for the retry.
                let proxy_baseline = self.settings_baseline.proxy.clone();
                self.settings_baseline = self.settings_working_copy();
                self.settings_baseline.proxy = proxy_baseline;
                self.notify(format!(
                    "saved defaults to {} — proxy not persisted (contains credentials)",
                    path.display()
                ));
            }
            Ok(_) => {
                self.settings_touched.clear();
                self.settings_baseline = self.settings_working_copy();
                self.notify(format!("saved defaults to {}", path.display()));
            }
            Err(err) => self.set_settings_message(format!("save failed — {err}")),
        }
    }

    /// A snapshot of every session-scoped value the panel manages, taken from
    /// the app's current state — used both to open the panel and (via
    /// [`Self::refresh_settings_panel`]) to refresh its mirror after a change.
    /// `persisted` is a best-effort dirty-indicator baseline: freshly re-read
    /// from disk every time (config.toml is tiny), so it never goes stale
    /// across a session even if something else edits the file; a read/parse
    /// failure degrades to the built-in defaults rather than blocking the
    /// panel (the indicator is advisory, never load-bearing). `touched`
    /// mirrors [`Self::settings_touched`] — see
    /// [`crate::tui::components::settings::SettingKey`]'s doc.
    fn settings_snapshot(&self) -> crate::tui::components::settings::SettingsSnapshot {
        let persisted = churl_core::config::resolve_settings_path()
            .and_then(|path| churl_core::config::load_config(&path))
            .and_then(|config| churl_core::config::ResolvedSettings::from_config(&config))
            .unwrap_or_default();
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
            persisted,
            touched: self.settings_touched.clone(),
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
