//! Key handling for the Options overlay: row navigation, the proxy inline edit,
//! the toggles, and cookie-list delete/clear. Split out of `options/mod.rs` so
//! its `impl OptionsState` keeps full access to the state's private helpers
//! without visibility widening.

use crossterm::event::{KeyCode, KeyEvent};

use super::{LineEditor, OptionsFocus, OptionsOutcome, OptionsRow, OptionsState};

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
        }
    }

    /// Keys while the three control rows have focus.
    fn handle_rows_key(&mut self, key: KeyEvent) -> OptionsOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.row = match self.row {
                    OptionsRow::Proxy => OptionsRow::Tls,
                    OptionsRow::Tls => OptionsRow::Cookies,
                    OptionsRow::Cookies => OptionsRow::Cookies,
                };
                OptionsOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.row = match self.row {
                    OptionsRow::Proxy => OptionsRow::Proxy,
                    OptionsRow::Tls => OptionsRow::Proxy,
                    OptionsRow::Cookies => OptionsRow::Tls,
                };
                OptionsOutcome::Consumed
            }
            // Descend into the cookie list (only meaningful on the Cookies row with
            // cookies present).
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.row == OptionsRow::Cookies && !self.cookies.is_empty() =>
            {
                self.focus = OptionsFocus::CookieList;
                self.cookie_sel = 0;
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

    /// Routes a paste into the proxy inline editor when it is open (the only text
    /// surface in this overlay). Returns `true` when consumed, `false` otherwise
    /// (the row list / cookie list take no free text).
    pub fn paste(&mut self, text: &str) -> bool {
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
