//! Key handling for the Options overlay: row navigation, the proxy inline edit,
//! the toggles, and cookie-list delete/clear. Split out of `options/mod.rs` so
//! its `impl OptionsState` keeps full access to the state's private helpers
//! without visibility widening.

use crossterm::event::{KeyCode, KeyEvent};

use super::{AdvancedField, LineEditor, OptionsFocus, OptionsOutcome, OptionsRow, OptionsState};

impl OptionsState {
    /// Handles one key event, returning what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        // A live message is cleared on the next interaction so it does not linger.
        self.message = None;

        if self.editing.is_some() {
            return self.handle_proxy_edit_key(key);
        }
        match self.focus {
            OptionsFocus::Rows => self.handle_rows_key(key),
            OptionsFocus::CookieList => self.handle_cookie_list_key(key),
            OptionsFocus::AdvancedList => self.handle_advanced_key(key),
        }
    }

    /// Keys while the top control rows have focus. [`OptionsRow::Advanced`]
    /// is only ever reached via `j`/Down from Cookies when
    /// [`OptionsState::debug_enabled`] — the guard makes it functionally
    /// unreachable (not merely unrendered) outside a debug session, so
    /// non-debug behaviour is byte-identical to before M8.3 Wave 4.
    fn handle_rows_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.row = match self.row {
                    OptionsRow::Proxy => OptionsRow::Tls,
                    OptionsRow::Tls => OptionsRow::Cookies,
                    OptionsRow::Cookies if self.debug_enabled => OptionsRow::Advanced,
                    OptionsRow::Cookies => OptionsRow::Cookies,
                    OptionsRow::Advanced => OptionsRow::Advanced,
                };
                OptionsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.row = match self.row {
                    OptionsRow::Proxy => OptionsRow::Proxy,
                    OptionsRow::Tls => OptionsRow::Proxy,
                    OptionsRow::Cookies => OptionsRow::Tls,
                    OptionsRow::Advanced => OptionsRow::Cookies,
                };
                OptionsOutcome::Consumed
            }
            // Descend into the cookie list (only meaningful on the Cookies row with
            // cookies present) or the Advanced field list.
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.row == OptionsRow::Cookies && !self.cookies.is_empty() =>
            {
                self.focus = OptionsFocus::CookieList;
                self.cookie_sel = 0;
                OptionsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.row == OptionsRow::Advanced =>
            {
                self.focus = OptionsFocus::AdvancedList;
                OptionsOutcome::Consumed
            }
            // Activate the selected row.
            KeyCode::Enter | KeyCode::Char(' ') => match self.row {
                OptionsRow::Proxy => {
                    // Edit the real value (not the masked display).
                    self.editing = Some(LineEditor::new(self.proxy.as_deref().unwrap_or("")));
                    OptionsOutcome::Consumed
                }
                OptionsRow::Tls => OptionsOutcome::ToggleInsecure,
                OptionsRow::Cookies => OptionsOutcome::ToggleCookies,
                OptionsRow::Advanced => {
                    self.focus = OptionsFocus::AdvancedList;
                    OptionsOutcome::Consumed
                }
            },
            // `i` also edits the proxy row (vim-ish); a no-op elsewhere.
            KeyCode::Char('i') if self.row == OptionsRow::Proxy => {
                self.editing = Some(LineEditor::new(self.proxy.as_deref().unwrap_or("")));
                OptionsOutcome::Consumed
            }
            KeyCode::Char('q') | KeyCode::Esc => OptionsOutcome::Close,
            _ => OptionsOutcome::Consumed,
        }
    }

    /// Keys while the Advanced field list has focus (`concurrency` / `total`
    /// / `body cap` / `timeout`).
    fn handle_advanced_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        if self.advanced_editing.is_some() {
            return self.handle_advanced_edit_key(key);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.advanced_field = self.advanced_field.next();
                OptionsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.advanced_field = self.advanced_field.prev();
                OptionsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = OptionsFocus::Rows;
                OptionsOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                self.begin_advanced_edit();
                OptionsOutcome::Consumed
            }
            KeyCode::Char('q') | KeyCode::Esc => OptionsOutcome::Close,
            _ => OptionsOutcome::Consumed,
        }
    }

    /// Seeds the numeric editor with the focused Advanced field's current value.
    fn begin_advanced_edit(&mut self) {
        let seed = match self.advanced_field {
            AdvancedField::Concurrency => self.advanced.concurrency.to_string(),
            AdvancedField::Total => self.advanced.total.to_string(),
            AdvancedField::BodyCapBytes => self.advanced.body_cap_bytes.to_string(),
            AdvancedField::TimeoutSecs => self.advanced.timeout_secs.to_string(),
        };
        self.advanced_editing = Some(LineEditor::new(&seed));
    }

    /// Advanced-field-edit keys: digits only (mirrors the load runner's
    /// numeric field editor); Enter commits, Esc cancels.
    fn handle_advanced_edit_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        match key.code {
            KeyCode::Enter => self.commit_advanced_edit(),
            KeyCode::Esc => {
                self.advanced_editing = None;
                OptionsOutcome::Consumed
            }
            KeyCode::Char(c) if !c.is_ascii_digit() => OptionsOutcome::Consumed,
            _ => {
                if let Some(editor) = self.advanced_editing.as_mut() {
                    editor.handle_key(key);
                }
                OptionsOutcome::Consumed
            }
        }
    }

    /// Commits the focused Advanced field's edit: parses the digits and
    /// requires a positive value (a zero concurrency/total/cap/timeout would
    /// be a footgun, not a meaningful override) — an empty, unparseable, or
    /// zero value is rejected with an inline message and the field stays
    /// unchanged. A valid value emits [`OptionsOutcome::ApplyAdvanced`] for
    /// the app to guardrail-check (concurrency/total) and apply.
    fn commit_advanced_edit(&mut self) -> OptionsOutcome {
        let Some(editor) = self.advanced_editing.take() else {
            return OptionsOutcome::Consumed;
        };
        let text = editor.text();
        match text.trim().parse::<u64>() {
            Ok(value) if value > 0 => OptionsOutcome::ApplyAdvanced {
                field: self.advanced_field,
                value,
            },
            _ => {
                self.message = Some("advanced value must be a positive whole number".to_owned());
                OptionsOutcome::Consumed
            }
        }
    }

    /// Routes a paste into the proxy inline editor when it is open (the only text
    /// surface in this overlay). Returns `true` when consumed, `false` otherwise
    /// (the row list / cookie list take no free text).
    pub fn paste(&mut self, text: &str) -> bool {
        if let Some(editor) = self.advanced_editing.as_mut() {
            // Digits only, mirroring the load runner's numeric-field paste —
            // keeps the field a valid number regardless of clipboard content.
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            editor.insert_str(&digits);
            return true;
        }
        let Some(editor) = self.editing.as_mut() else {
            return false;
        };
        editor.insert_str(text);
        true
    }

    /// Keys while the proxy inline edit is open.
    fn handle_proxy_edit_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        let Some(editor) = self.editing.as_mut() else {
            return OptionsOutcome::Consumed;
        };
        if editor.handle_key(key) {
            return OptionsOutcome::Consumed;
        }
        match key.code {
            KeyCode::Enter => {
                let text = editor.text();
                self.editing = None;
                let trimmed = text.trim();
                // An empty proxy clears it (env-proxy fallback).
                let proxy = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                };
                OptionsOutcome::ApplyProxy(proxy)
            }
            KeyCode::Esc => {
                self.editing = None;
                OptionsOutcome::Consumed
            }
            _ => OptionsOutcome::Consumed,
        }
    }

    /// Keys while the cookie list has focus.
    fn handle_cookie_list_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.cookie_sel + 1 < self.cookies.len() {
                    self.cookie_sel += 1;
                }
                OptionsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cookie_sel = self.cookie_sel.saturating_sub(1);
                OptionsOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = OptionsFocus::Rows;
                OptionsOutcome::Consumed
            }
            KeyCode::Char('d') => match self.selected_cookie() {
                Some((domain, name)) => OptionsOutcome::DeleteCookie { domain, name },
                None => OptionsOutcome::Consumed,
            },
            KeyCode::Char('x') => {
                if self.cookies.is_empty() {
                    OptionsOutcome::Consumed
                } else {
                    OptionsOutcome::ClearCookies
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => OptionsOutcome::Close,
            _ => OptionsOutcome::Consumed,
        }
    }
}
