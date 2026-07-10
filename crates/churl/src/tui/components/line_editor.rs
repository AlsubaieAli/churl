//! A minimal hand-rolled single-line text editor shared by the URL bar (inline
//! URL edit), the request-tab row editing (name/value fields), and the CRUD
//! prompts. edtui is overkill for one line and would pull its vim modality where
//! we want a plain input, so this is deliberately dependency-free.
//!
//! The editor works in *character* units (a `Vec<char>` buffer + a char cursor),
//! so multibyte input behaves. Rendering is left to the caller — the editor
//! exposes the current text and the cursor column so a component can draw a
//! block cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// A horizontal viewport slice of a [`LineEditor`], computed to keep the cursor
/// in view within a fixed display width. Rendered by the URL bar / prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorView {
    /// The visible characters (a contiguous slice of the buffer).
    pub text: String,
    /// The cursor's column *within the visible slice* (display cells), always in
    /// `0..=visible_width`.
    pub cursor_col: usize,
    /// Whether content is clipped to the left (render a `…` indicator).
    pub clipped_left: bool,
    /// Whether content is clipped to the right.
    pub clipped_right: bool,
}

/// The display width of a char (0 for control chars, so they never desync the
/// column math).
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Single-line editable text with a cursor. Insert chars, backspace/delete, move
/// the cursor (arrows, Home/End, Ctrl-A/Ctrl-E).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    chars: Vec<char>,
    /// Cursor position as a char index in `0..=chars.len()`.
    cursor: usize,
    /// The left edge of the horizontal viewport as a char index. Kept across
    /// calls so the view is stable (adjusted by [`LineEditor::view`]).
    scroll: usize,
}

impl LineEditor {
    /// Creates an editor seeded with `text`, cursor at the end.
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self {
            chars,
            cursor,
            scroll: 0,
        }
    }

    /// Computes the horizontal viewport for a display `width`, following the
    /// cursor. Mutates the internal scroll offset so the view is stable across
    /// calls but always keeps the cursor visible: if the cursor drifts off
    /// either edge the window shifts to bring it back, with `…` edge indicators
    /// when content is clipped. `width` counts display cells; the returned slice
    /// leaves room for the edge indicators.
    pub fn view(&mut self, width: usize) -> EditorView {
        if width == 0 {
            return EditorView {
                text: String::new(),
                cursor_col: 0,
                clipped_left: false,
                clipped_right: false,
            };
        }
        // Scroll left if the cursor moved before the window.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        // Scroll right until the cursor's cell fits inside `width` from `scroll`.
        // Cell width from `scroll` to (and including a caret cell at) `cursor`.
        loop {
            let used: usize = self.chars[self.scroll..self.cursor]
                .iter()
                .map(|c| char_width(*c))
                .sum::<usize>()
                + 1; // reserve one cell for the block cursor
            if used <= width || self.scroll >= self.cursor {
                break;
            }
            self.scroll += 1;
        }
        let mut text = String::new();
        let mut cells = 0usize;
        let mut end = self.scroll;
        while end < self.chars.len() {
            let w = char_width(self.chars[end]);
            if cells + w > width {
                break;
            }
            text.push(self.chars[end]);
            cells += w;
            end += 1;
        }
        // Cursor column within the visible slice (display cells).
        let cursor_col: usize = self.chars[self.scroll..self.cursor.min(end)]
            .iter()
            .map(|c| char_width(*c))
            .sum();
        EditorView {
            text,
            cursor_col,
            clipped_left: self.scroll > 0,
            clipped_right: end < self.chars.len(),
        }
    }

    /// The current text.
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// The cursor column (char offset from the start).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Handles one key event, mutating the buffer/cursor. Returns `true` when the
    /// key was consumed (so the caller does not treat it as a commit/cancel).
    /// `Enter`/`Esc`/`Tab` are *not* consumed here — they are control keys the
    /// owner interprets (commit/cancel/advance).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Char(c) if !ctrl => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.chars.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn insert_at_cursor_and_report_text() {
        let mut ed = LineEditor::new("");
        for c in "abc".chars() {
            assert!(ed.handle_key(key(KeyCode::Char(c))));
        }
        assert_eq!(ed.text(), "abc");
        assert_eq!(ed.cursor(), 3);
    }

    #[test]
    fn seeded_cursor_at_end() {
        let ed = LineEditor::new("hello");
        assert_eq!(ed.cursor(), 5);
        assert_eq!(ed.text(), "hello");
    }

    #[test]
    fn backspace_and_delete() {
        let mut ed = LineEditor::new("abcd");
        ed.handle_key(key(KeyCode::Backspace)); // removes 'd'
        assert_eq!(ed.text(), "abc");
        ed.handle_key(key(KeyCode::Home));
        ed.handle_key(key(KeyCode::Delete)); // removes 'a'
        assert_eq!(ed.text(), "bc");
        assert_eq!(ed.cursor(), 0);
        // Backspace at start is a no-op.
        ed.handle_key(key(KeyCode::Backspace));
        assert_eq!(ed.text(), "bc");
    }

    #[test]
    fn motion_arrows_home_end_and_ctrl() {
        let mut ed = LineEditor::new("hello");
        ed.handle_key(key(KeyCode::Left));
        ed.handle_key(key(KeyCode::Left));
        assert_eq!(ed.cursor(), 3);
        ed.handle_key(ctrl('a'));
        assert_eq!(ed.cursor(), 0);
        ed.handle_key(ctrl('e'));
        assert_eq!(ed.cursor(), 5);
        ed.handle_key(key(KeyCode::Right)); // clamped at end
        assert_eq!(ed.cursor(), 5);
        // Insert in the middle.
        ed.handle_key(key(KeyCode::Home));
        assert!(ed.handle_key(key(KeyCode::Char('X'))));
        assert_eq!(ed.text(), "Xhello");
        assert_eq!(ed.cursor(), 1);
    }

    #[test]
    fn unicode_chars_are_char_indexed() {
        let mut ed = LineEditor::new("café");
        assert_eq!(ed.cursor(), 4, "4 chars, not bytes");
        ed.handle_key(key(KeyCode::Backspace));
        assert_eq!(ed.text(), "caf");
        // Insert a multibyte char.
        assert!(ed.handle_key(key(KeyCode::Char('é'))));
        assert!(ed.handle_key(key(KeyCode::Char('日'))));
        assert_eq!(ed.text(), "café日");
    }

    #[test]
    fn viewport_short_text_fits_no_clip() {
        let mut ed = LineEditor::new("abc");
        let view = ed.view(10);
        assert_eq!(view.text, "abc");
        assert_eq!(view.cursor_col, 3);
        assert!(!view.clipped_left);
        assert!(!view.clipped_right);
    }

    #[test]
    fn viewport_follows_cursor_right() {
        // 20-char string, cursor at end, width 5 → window shows the tail.
        let mut ed = LineEditor::new("0123456789abcdefghij");
        let view = ed.view(5);
        assert!(view.clipped_left, "left edge must show clip when scrolled");
        assert!(!view.clipped_right, "cursor at end → nothing clipped right");
        // Cursor stays within the visible width.
        assert!(view.cursor_col <= 5);
    }

    #[test]
    fn viewport_follows_cursor_left() {
        let mut ed = LineEditor::new("0123456789abcdefghij");
        ed.view(5); // scroll to the right first
        for _ in 0..20 {
            ed.handle_key(key(KeyCode::Left));
        }
        let view = ed.view(5);
        assert_eq!(view.cursor_col, 0);
        assert!(!view.clipped_left, "cursor at start → nothing clipped left");
        assert!(view.clipped_right, "long tail must clip right");
        assert!(view.text.starts_with('0'));
    }

    #[test]
    fn viewport_unicode_widths() {
        // Wide (2-cell) CJK chars: width 4 fits two of them.
        let mut ed = LineEditor::new("日本語");
        ed.handle_key(key(KeyCode::Home));
        let view = ed.view(4);
        // Two wide chars = 4 cells; the third clips.
        assert_eq!(view.text, "日本");
        assert!(view.clipped_right);
        assert_eq!(view.cursor_col, 0);
    }

    #[test]
    fn control_keys_not_consumed_for_owner() {
        let mut ed = LineEditor::new("x");
        assert!(!ed.handle_key(key(KeyCode::Enter)));
        assert!(!ed.handle_key(key(KeyCode::Esc)));
        assert!(!ed.handle_key(key(KeyCode::Tab)));
        assert_eq!(ed.text(), "x");
    }
}
