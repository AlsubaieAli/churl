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

/// Single-line editable text with a cursor. Insert chars, backspace/delete, move
/// the cursor (arrows, Home/End, Ctrl-A/Ctrl-E).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    chars: Vec<char>,
    /// Cursor position as a char index in `0..=chars.len()`.
    cursor: usize,
}

impl LineEditor {
    /// Creates an editor seeded with `text`, cursor at the end.
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
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
    fn control_keys_not_consumed_for_owner() {
        let mut ed = LineEditor::new("x");
        assert!(!ed.handle_key(key(KeyCode::Enter)));
        assert!(!ed.handle_key(key(KeyCode::Esc)));
        assert!(!ed.handle_key(key(KeyCode::Tab)));
        assert_eq!(ed.text(), "x");
    }
}
