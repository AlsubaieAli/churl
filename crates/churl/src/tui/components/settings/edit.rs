//! Key handling for the Settings panel: menu navigation, per-category row
//! navigation, the text/numeric edit sub-state, and the cookie/advanced lists.
//! Split out of `settings/mod.rs` so its `impl SettingsState` keeps full access
//! to the state's private helpers without visibility widening.

use std::str::FromStr;

use churl_core::config::{RedirectPolicy, UrlEditMode};
use churl_core::secrets::SecretPolicy;
use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyEvent};

use super::{
    AdvancedField, AppearanceRow, DebugRow, EditTarget, LineEditor, NetworkRow, PanelFocus,
    RequestRow, SettingsCategory, SettingsLevel, SettingsOutcome, SettingsState,
};

impl SettingsState {
    /// Handles one key event, returning what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        // A live message is cleared on the next interaction so it does not linger.
        self.message = None;

        if self.cookie_form.is_some() {
            return self.handle_cookie_form_key(key);
        }
        if self.editing.is_some() {
            return self.handle_edit_key(key);
        }
        // `s` saves the current working copy as the default (M8.5 Wave 3),
        // reachable from anywhere in the panel — mirroring `q`'s reach-from-
        // anywhere close. Free at every level (no row/list binds it) and never
        // shadows typed text (an open edit returns above, before this check).
        if matches!(key.code, KeyCode::Char('s')) {
            return SettingsOutcome::SaveDefaults;
        }
        match self.level {
            SettingsLevel::Menu => self.handle_menu_key(key),
            SettingsLevel::Panel => match self.focus {
                PanelFocus::Rows => self.handle_panel_rows_key(key),
                PanelFocus::CookieList => self.handle_cookie_list_key(key),
                PanelFocus::AdvancedList => self.handle_advanced_list_key(key),
            },
        }
    }

    // ---- Level 1: category menu ----

    fn handle_menu_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        let visible = SettingsCategory::visible(self.debug_enabled);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.category = cycle_in(&visible, self.category, 1);
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.category = cycle_in(&visible, self.category, -1);
                SettingsOutcome::Consumed
            }
            KeyCode::Enter
            | KeyCode::Char(' ')
            | KeyCode::Tab
            | KeyCode::Char('l')
            | KeyCode::Right => {
                self.level = SettingsLevel::Panel;
                self.focus = PanelFocus::Rows;
                SettingsOutcome::Consumed
            }
            KeyCode::Char('q') | KeyCode::Esc => SettingsOutcome::Close,
            _ => SettingsOutcome::Consumed,
        }
    }

    // ---- Level 2: the active category's rows ----

    fn handle_panel_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        // `q` closes the whole panel from anywhere; `Esc` backs up ONE level
        // (Panel/Rows → Menu) — the two are deliberately different depths.
        if matches!(key.code, KeyCode::Char('q')) {
            return SettingsOutcome::Close;
        }
        if matches!(key.code, KeyCode::Esc) {
            self.level = SettingsLevel::Menu;
            return SettingsOutcome::Consumed;
        }
        match self.category {
            SettingsCategory::Request => self.handle_request_rows_key(key),
            SettingsCategory::Network => self.handle_network_rows_key(key),
            SettingsCategory::Load => self.handle_load_rows_key(key),
            SettingsCategory::Appearance => self.handle_appearance_rows_key(key),
            SettingsCategory::Debug => self.handle_debug_rows_key(key),
        }
    }

    // ---- Request category ----

    fn handle_request_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.request_row = self.request_row.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.request_row = self.request_row.prev();
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('i') => match self.request_row {
                RequestRow::Timeout => {
                    self.begin_advanced_edit(AdvancedField::TimeoutSecs);
                    SettingsOutcome::Consumed
                }
                RequestRow::MaxBodyBytes => {
                    self.begin_advanced_edit(AdvancedField::BodyCapBytes);
                    SettingsOutcome::Consumed
                }
                RequestRow::Redirect => {
                    self.redirect = next_redirect(self.redirect);
                    SettingsOutcome::ApplyRedirect(self.redirect)
                }
                RequestRow::UrlEdit => {
                    self.url_edit = next_url_edit(self.url_edit);
                    SettingsOutcome::ApplyUrlEdit(self.url_edit)
                }
                RequestRow::SecretPolicy => {
                    self.secret_policy = next_secret_policy(self.secret_policy);
                    SettingsOutcome::ApplySecretPolicy(self.secret_policy)
                }
            },
            _ => SettingsOutcome::Consumed,
        }
    }

    // ---- Network category (ported from the old Options overlay) ----

    fn handle_network_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.network_row = self.network_row.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.network_row = self.network_row.prev();
                SettingsOutcome::Consumed
            }
            // Descend into the cookie list (only meaningful on the Cookies row
            // with cookies present).
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.network_row == NetworkRow::Cookies && !self.cookies.is_empty() =>
            {
                self.focus = PanelFocus::CookieList;
                self.cookie_sel = 0;
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.network_row {
                NetworkRow::Proxy => {
                    self.editing = Some((
                        EditTarget::Proxy,
                        LineEditor::new(self.proxy.as_deref().unwrap_or("")),
                    ));
                    SettingsOutcome::Consumed
                }
                NetworkRow::Tls => SettingsOutcome::ToggleInsecure,
                NetworkRow::Cookies => SettingsOutcome::ToggleCookies,
            },
            KeyCode::Char('i') if self.network_row == NetworkRow::Proxy => {
                self.editing = Some((
                    EditTarget::Proxy,
                    LineEditor::new(self.proxy.as_deref().unwrap_or("")),
                ));
                SettingsOutcome::Consumed
            }
            KeyCode::Char('a') if self.network_row == NetworkRow::Cookies => {
                self.open_add_cookie_form();
                SettingsOutcome::Consumed
            }
            _ => SettingsOutcome::Consumed,
        }
    }

    fn handle_cookie_list_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.cookie_sel + 1 < self.cookies.len() {
                    self.cookie_sel += 1;
                }
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cookie_sel = self.cookie_sel.saturating_sub(1);
                SettingsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                self.focus = PanelFocus::Rows;
                SettingsOutcome::Consumed
            }
            KeyCode::Char('d') => match self.selected_cookie() {
                Some((domain, name)) => SettingsOutcome::DeleteCookie { domain, name },
                None => SettingsOutcome::Consumed,
            },
            KeyCode::Char('x') => {
                if self.cookies.is_empty() {
                    SettingsOutcome::Consumed
                } else {
                    SettingsOutcome::ClearCookies
                }
            }
            KeyCode::Char('a') => {
                self.open_add_cookie_form();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('e') => {
                self.open_edit_cookie_form();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('q') => SettingsOutcome::Close,
            _ => SettingsOutcome::Consumed,
        }
    }

    // ---- Load category ----

    fn handle_load_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.load_row = self.load_row.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.load_row = self.load_row.prev();
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('i') => {
                let seed = self.load_row.get(&self.load_caps).to_string();
                self.editing = Some((EditTarget::Load(self.load_row), LineEditor::new(&seed)));
                SettingsOutcome::Consumed
            }
            _ => SettingsOutcome::Consumed,
        }
    }

    // ---- Appearance category ----

    fn handle_appearance_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.appearance_row = self.appearance_row.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.appearance_row = self.appearance_row.prev();
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('i') => match self.appearance_row {
                AppearanceRow::Theme => {
                    let next = if self.theme_name == "light" {
                        "dark"
                    } else {
                        "light"
                    };
                    self.theme_name = next.to_owned();
                    SettingsOutcome::ApplyTheme(next.to_owned())
                }
                AppearanceRow::LeaderKey => {
                    self.editing = Some((EditTarget::LeaderKey, LineEditor::new(&self.leader_key)));
                    SettingsOutcome::Consumed
                }
            },
            _ => SettingsOutcome::Consumed,
        }
    }

    // ---- Debug category (ported from the old Options overlay's Advanced section) ----

    fn handle_debug_rows_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.debug_row = self.debug_row.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.debug_row = self.debug_row.prev();
                SettingsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.debug_row == DebugRow::Advanced =>
            {
                self.focus = PanelFocus::AdvancedList;
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.debug_row {
                DebugRow::DebugToggle => SettingsOutcome::ToggleDebug,
                DebugRow::Advanced => {
                    self.focus = PanelFocus::AdvancedList;
                    SettingsOutcome::Consumed
                }
            },
            _ => SettingsOutcome::Consumed,
        }
    }

    /// Keys while the Advanced field list has focus (`concurrency` / `total`
    /// / `body cap` / `timeout`).
    fn handle_advanced_list_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.advanced_field = self.advanced_field.next();
                SettingsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.advanced_field = self.advanced_field.prev();
                SettingsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = PanelFocus::Rows;
                SettingsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                self.begin_advanced_edit(self.advanced_field);
                SettingsOutcome::Consumed
            }
            KeyCode::Char('q') => SettingsOutcome::Close,
            KeyCode::Esc => {
                self.focus = PanelFocus::Rows;
                SettingsOutcome::Consumed
            }
            _ => SettingsOutcome::Consumed,
        }
    }

    /// Seeds the numeric editor with `field`'s current resolved value.
    fn begin_advanced_edit(&mut self, field: AdvancedField) {
        let seed = field.get(&self.advanced).to_string();
        self.editing = Some((EditTarget::Advanced(field), LineEditor::new(&seed)));
    }

    // ---- Level 3: the open text/numeric edit ----

    /// Routes a paste into the open editor (the only text surface in this
    /// panel). Returns `true` when consumed, `false` otherwise (menu/rows/list
    /// navigation takes no free text).
    pub fn paste(&mut self, text: &str) -> bool {
        let Some((target, editor)) = self.editing.as_mut() else {
            return false;
        };
        match target {
            EditTarget::Advanced(_) | EditTarget::Load(_) => {
                // Digits only, mirroring the load runner's numeric-field paste —
                // keeps the field a valid number regardless of clipboard content.
                let digits: String = text.chars().filter(char::is_ascii_digit).collect();
                editor.insert_str(&digits);
            }
            EditTarget::Proxy | EditTarget::LeaderKey => editor.insert_str(text),
        }
        true
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        let Some((target, editor)) = self.editing.as_mut() else {
            return SettingsOutcome::Consumed;
        };
        if editor.handle_key(key) {
            return SettingsOutcome::Consumed;
        }
        let target = *target;
        match key.code {
            KeyCode::Enter => self.commit_edit(target),
            KeyCode::Esc => {
                self.editing = None;
                SettingsOutcome::Consumed
            }
            KeyCode::Char(c)
                if matches!(target, EditTarget::Advanced(_) | EditTarget::Load(_))
                    && !c.is_ascii_digit() =>
            {
                SettingsOutcome::Consumed
            }
            _ => SettingsOutcome::Consumed,
        }
    }

    /// Commits the open edit. Numeric targets require a positive whole number
    /// (an empty, unparseable, or zero value is rejected with an inline
    /// message, and the editor still closes — matching the old Advanced-field
    /// edit's behaviour exactly: retry means re-opening the row). The proxy
    /// text edit always commits (empty clears it); the leader-key text edit
    /// is validated as a parseable key combination (via the same `crokey`
    /// parser the real keymap uses) before committing.
    fn commit_edit(&mut self, target: EditTarget) -> SettingsOutcome {
        let Some((_, editor)) = self.editing.take() else {
            return SettingsOutcome::Consumed;
        };
        let text = editor.text();
        match target {
            EditTarget::Proxy => {
                let trimmed = text.trim();
                let proxy = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                };
                SettingsOutcome::ApplyProxy(proxy)
            }
            EditTarget::Advanced(field) => match text.trim().parse::<u64>() {
                Ok(value) if value > 0 => SettingsOutcome::ApplyAdvanced { field, value },
                _ => {
                    self.message = Some("value must be a positive whole number".to_owned());
                    SettingsOutcome::Consumed
                }
            },
            EditTarget::Load(field) => match text.trim().parse::<usize>() {
                Ok(value) if value > 0 => SettingsOutcome::ApplyLoadCap { field, value },
                _ => {
                    self.message = Some("value must be a positive whole number".to_owned());
                    SettingsOutcome::Consumed
                }
            },
            EditTarget::LeaderKey => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    self.message = Some("leader key cannot be empty".to_owned());
                    return SettingsOutcome::Consumed;
                }
                match KeyCombination::from_str(trimmed) {
                    Ok(_) => SettingsOutcome::ApplyLeaderKey(trimmed.to_owned()),
                    Err(err) => {
                        self.message = Some(format!("bad key combination: {err}"));
                        SettingsOutcome::Consumed
                    }
                }
            }
        }
    }
}

/// Cycles `current` within `visible` (which may be a strict subset of the
/// type's full variant set — the Menu's category list, filtered by debug) by
/// `delta` (`1` = next, `-1` = prev), wrapping. Falls back to the first
/// element if `current` is not in `visible` at all (defensive; should not
/// happen in practice).
fn cycle_in<T: Copy + PartialEq>(visible: &[T], current: T, delta: i32) -> T {
    if visible.is_empty() {
        return current;
    }
    let idx = visible.iter().position(|v| *v == current).unwrap_or(0) as i32;
    let len = visible.len() as i32;
    let next = ((idx + delta) % len + len) % len;
    visible[next as usize]
}

fn next_redirect(policy: RedirectPolicy) -> RedirectPolicy {
    match policy {
        RedirectPolicy::Strip => RedirectPolicy::Strict,
        RedirectPolicy::Strict => RedirectPolicy::FollowAll,
        RedirectPolicy::FollowAll => RedirectPolicy::Strip,
    }
}

fn next_url_edit(mode: UrlEditMode) -> UrlEditMode {
    match mode {
        UrlEditMode::Inline => UrlEditMode::Popup,
        UrlEditMode::Popup => UrlEditMode::Inline,
    }
}

fn next_secret_policy(policy: SecretPolicy) -> SecretPolicy {
    match policy {
        SecretPolicy::Strict => SecretPolicy::Warn,
        SecretPolicy::Warn => SecretPolicy::Strict,
    }
}
