//! Help-overlay key handling + in-help search, extracted from `app.rs`
//! (M7.11). Grandchild module of `app`; `impl App` here keeps full access to
//! `App`'s private fields and methods without any visibility widening.

use super::super::*;

impl App {
    /// Handles one key while the `?` help overlay is open: `?`/Esc/`q` close;
    /// `j`/`k`/arrows scroll.
    pub(in crate::tui::app) fn handle_help_key(&mut self, key: KeyEvent) -> Result<()> {
        // While the `/` search input is open, every keystroke feeds the search.
        if self.help_search_input {
            self.handle_help_search_key(key);
            return Ok(());
        }
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                self.help_open = false;
                self.help_scroll = 0;
                self.help_search = None;
                self.help_search_input = false;
            }
            KeyCode::Char('/') => self.open_help_search(),
            KeyCode::Char('n') if self.help_search.is_some() => self.help_search_step(true),
            KeyCode::Char('N') if self.help_search.is_some() => self.help_search_step(false),
            KeyCode::Char('j') | KeyCode::Down => self.help_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') => {
                let half = (self.help_viewport_height / 2).max(1);
                self.help_scroll += half;
            }
            KeyCode::Char('u') => {
                let half = (self.help_viewport_height / 2).max(1);
                self.help_scroll = self.help_scroll.saturating_sub(half);
            }
            _ => {}
        }
        Ok(())
    }

    /// `/` inside the help overlay: open the incremental help-search input
    /// (mirrors [`Self::open_body_search`]). Seeds an empty live search so
    /// highlighting engages immediately.
    pub(in crate::tui::app) fn open_help_search(&mut self) {
        self.help_search_editor = LineEditor::new("");
        let mut search = help::HelpSearch::default();
        search.set_query(String::new(), &self.keymap, &self.theme);
        self.help_search = Some(search);
        self.help_search_input = true;
    }

    /// `n`/`N` inside the help overlay: step to the next/previous match
    /// (wrapping), scrolling it into view. Mirrors [`Self::response_search_step`].
    pub(in crate::tui::app) fn help_search_step(&mut self, forward: bool) {
        let stepped = self
            .help_search
            .as_mut()
            .and_then(|s| s.step(forward))
            .is_some();
        if stepped {
            self.help_center_on_match();
        }
    }

    /// Scrolls the help overlay so the current search match's line is visible,
    /// keeping it within the last-rendered viewport (jump-to-match).
    pub(in crate::tui::app) fn help_center_on_match(&mut self) {
        let Some(line) = self.help_search.as_ref().and_then(|s| s.current_line()) else {
            return;
        };
        let vh = self.help_viewport_height.max(1);
        // Bring the match into the viewport.
        if line < self.help_scroll {
            self.help_scroll = line;
        } else if line >= self.help_scroll + vh {
            self.help_scroll = line + 1 - vh;
        }
    }
}
