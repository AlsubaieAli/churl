//! Normal-mode motion extensions edtui 0.11.3 lacks: `W`/`B` (WORD motions),
//! `^` (first non-blank), and the `f`/`F`/`t`/`T` find-char family. edtui's key
//! register cannot express `f<any-char>` (it maps fixed key sequences) and
//! upstream binds first-non-blank only as `_`; so these motions are implemented
//! churl-side as cursor mutations on `EditorState` (DECISIONS.md). Applied
//! uniformly to both edtui editors (URL popup + Body tab).
//!
//! Only meaningful in `EditorMode::Normal`; the caller guards mode. Cols are
//! char positions (`Jagged<char>` is char-indexed), so unicode needs no byte
//! math. The cursor col stays clamped to the row's last char (Normal-mode
//! convention edtui uses).

use crossterm::event::{KeyCode, KeyEvent};
use edtui::{EditorState, Index2};

/// A pending find awaiting its target char.
#[derive(Clone, Copy)]
struct Find {
    /// `false` = `f`/`t` (forward), `true` = `F`/`T` (backward).
    backward: bool,
    /// `true` = `t`/`T` (till: land one col short of the target).
    till: bool,
}

/// Per-editor pending find state (`f`/`F`/`t`/`T` awaiting their target char).
#[derive(Default)]
pub struct VimExt {
    pending: Option<Find>,
}

impl VimExt {
    /// Clears any pending find. Called when the editor is (re)opened so a stale
    /// half-typed find never leaks into the next session.
    pub fn reset(&mut self) {
        self.pending = None;
    }
}

/// Handles one Normal-mode key. Returns `true` when consumed (motion applied or
/// a pending find resolved/aborted), `false` when the key is none of ours and
/// should fall through to the caller's normal routing.
pub fn handle_key(key: KeyEvent, state: &mut EditorState, ext: &mut VimExt) -> bool {
    // Only bare (or shifted) chars are motion input; a Ctrl/Alt-modified key is
    // never a target char or a motion starter (Ctrl-f must not arm a find, and
    // a pending find must not swallow Ctrl-s as a search for 's').
    let plain_char = match key.code {
        KeyCode::Char(c) if (key.modifiers - crossterm::event::KeyModifiers::SHIFT).is_empty() => {
            Some(c)
        }
        _ => None,
    };
    // A pending find claims the next key: a plain char resolves it; anything
    // else (Esc, a modified key, …) aborts it. Either way the key is consumed.
    if let Some(find) = ext.pending.take() {
        if let Some(c) = plain_char {
            apply_find(state, find, c);
        }
        return true;
    }

    let Some(c) = plain_char else {
        return false;
    };
    match c {
        'W' => {
            move_word_forward(state);
            true
        }
        'B' => {
            move_word_backward(state);
            true
        }
        '^' => {
            first_non_blank(state);
            true
        }
        'f' => set_pending(ext, false, false),
        'F' => set_pending(ext, true, false),
        't' => set_pending(ext, false, true),
        'T' => set_pending(ext, true, true),
        _ => false,
    }
}

fn set_pending(ext: &mut VimExt, backward: bool, till: bool) -> bool {
    ext.pending = Some(Find { backward, till });
    true
}

/// Char at `(row, col)`, or `None` when out of bounds.
fn char_at(state: &EditorState, row: usize, col: usize) -> Option<char> {
    state.lines.get(Index2::new(row, col)).copied()
}

/// Char count of `row` (0 for a non-existent or empty row).
fn row_len(state: &EditorState, row: usize) -> usize {
    state.lines.len_col(row).unwrap_or(0)
}

/// Last valid cursor col on `row` in Normal mode (row_len - 1, floored at 0).
fn last_col(state: &EditorState, row: usize) -> usize {
    row_len(state, row).saturating_sub(1)
}

/// Clamps the cursor col to the current row's last char (Normal-mode convention).
fn clamp_cursor(state: &mut EditorState) {
    let max = last_col(state, state.cursor.row);
    if state.cursor.col > max {
        state.cursor.col = max;
    }
}

/// `f<c>`/`F<c>`/`t<c>`/`T<c>`: current row only, strictly after/before the
/// cursor col. No match leaves the cursor unchanged.
fn apply_find(state: &mut EditorState, find: Find, target: char) {
    let row = state.cursor.row;
    let col = state.cursor.col;
    let len = row_len(state, row);
    let hit = if find.backward {
        (0..col)
            .rev()
            .find(|&i| char_at(state, row, i) == Some(target))
    } else {
        ((col + 1)..len).find(|&i| char_at(state, row, i) == Some(target))
    };
    let Some(mut dest) = hit else {
        return;
    };
    if find.till {
        // Land one col short of the target (toward the cursor).
        dest = if find.backward { dest + 1 } else { dest - 1 };
    }
    state.cursor.col = dest;
}

/// `^`: first non-whitespace col of the current row (col 0 if all whitespace or
/// empty).
fn first_non_blank(state: &mut EditorState) {
    let row = state.cursor.row;
    let len = row_len(state, row);
    let col = (0..len)
        .find(|&i| char_at(state, row, i).is_some_and(|c| !c.is_whitespace()))
        .unwrap_or(0);
    state.cursor.col = col;
    clamp_cursor(state);
}

/// `W`: next WORD start (WORD = non-whitespace run). Crosses lines forward:
/// with no later WORD on the row, advances to the next non-empty row's first
/// WORD (an empty row counts as a WORD stop). Clamps at the last char of the
/// last row.
fn move_word_forward(state: &mut EditorState) {
    let last_row = state.lines.len().saturating_sub(1);
    let mut row = state.cursor.row;
    let mut col = state.cursor.col;

    // Skip the current WORD (the run of non-whitespace under the cursor).
    while col < row_len(state, row) && !is_blank_at(state, row, col) {
        col += 1;
    }
    // Skip whitespace to the next WORD, crossing rows as needed.
    loop {
        if col >= row_len(state, row) {
            // End of row: advance to the next row.
            if row >= last_row {
                // Nothing further — clamp at the last char of the last row.
                state.cursor.row = last_row;
                state.cursor.col = last_col(state, last_row);
                return;
            }
            row += 1;
            col = 0;
            // An empty row is itself a WORD stop.
            if row_len(state, row) == 0 {
                state.cursor.row = row;
                state.cursor.col = 0;
                return;
            }
            continue;
        }
        if is_blank_at(state, row, col) {
            col += 1;
        } else {
            state.cursor.row = row;
            state.cursor.col = col;
            return;
        }
    }
}

/// `B`: previous WORD start, crossing rows backward symmetrically. Clamps at
/// `(0, 0)`.
fn move_word_backward(state: &mut EditorState) {
    let mut row = state.cursor.row;
    let mut col = state.cursor.col;

    // Step one position back, crossing to the previous row's end (an empty row
    // is a WORD stop).
    if !step_back(state, &mut row, &mut col) {
        state.cursor.row = 0;
        state.cursor.col = 0;
        return;
    }
    if row_len(state, row) == 0 {
        state.cursor.row = row;
        state.cursor.col = 0;
        return;
    }
    // Skip whitespace backward onto a WORD char.
    while is_blank_at(state, row, col) {
        if !step_back(state, &mut row, &mut col) {
            state.cursor.row = 0;
            state.cursor.col = 0;
            return;
        }
        if row_len(state, row) == 0 {
            state.cursor.row = row;
            state.cursor.col = 0;
            return;
        }
    }
    // Walk back to the start of this WORD.
    while col > 0 && !is_blank_at(state, row, col - 1) {
        col -= 1;
    }
    state.cursor.row = row;
    state.cursor.col = col;
}

/// Moves `(row, col)` one position backward. Returns `false` at `(0, 0)`.
/// Crossing to a previous row lands on its last char (or col 0 when empty).
fn step_back(state: &EditorState, row: &mut usize, col: &mut usize) -> bool {
    if *col > 0 {
        *col -= 1;
        return true;
    }
    if *row == 0 {
        return false;
    }
    *row -= 1;
    *col = last_col(state, *row);
    true
}

/// Whether the char at `(row, col)` is whitespace (out-of-bounds counts as blank).
fn is_blank_at(state: &EditorState, row: usize, col: usize) -> bool {
    char_at(state, row, col).is_none_or(|c| c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use edtui::Lines;

    fn state(text: &str) -> EditorState {
        EditorState::new(Lines::from(text))
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn at(state: &EditorState) -> (usize, usize) {
        (state.cursor.row, state.cursor.col)
    }

    #[test]
    fn w_within_row() {
        let mut s = state("foo bar baz");
        let mut e = VimExt::default();
        assert!(handle_key(key('W'), &mut s, &mut e));
        assert_eq!(at(&s), (0, 4)); // start of "bar"
        handle_key(key('W'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 8)); // start of "baz"
    }

    #[test]
    fn w_multi_space_and_unicode() {
        let mut s = state("héllo   wörld");
        let mut e = VimExt::default();
        handle_key(key('W'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 8)); // char-index start of "wörld"
    }

    #[test]
    fn w_across_rows() {
        let mut s = state("foo\nbar");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 1); // inside "foo"
        handle_key(key('W'), &mut s, &mut e);
        assert_eq!(at(&s), (1, 0)); // first WORD of next row
    }

    #[test]
    fn w_empty_row_is_a_stop() {
        let mut s = state("foo\n\nbar");
        let mut e = VimExt::default();
        handle_key(key('W'), &mut s, &mut e);
        assert_eq!(at(&s), (1, 0)); // stops on the empty row
    }

    #[test]
    fn w_at_end_of_text_clamps() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 5); // inside "bar"
        handle_key(key('W'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 6)); // clamped at last char
    }

    #[test]
    fn b_within_row() {
        let mut s = state("foo bar baz");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 9); // inside "baz"
        handle_key(key('B'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 8)); // start of "baz"
        handle_key(key('B'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 4)); // start of "bar"
    }

    #[test]
    fn b_across_rows() {
        let mut s = state("foo\nbar");
        let mut e = VimExt::default();
        s.cursor = Index2::new(1, 1); // inside "bar"
        handle_key(key('B'), &mut s, &mut e);
        assert_eq!(at(&s), (1, 0)); // start of "bar"
        handle_key(key('B'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 0)); // back onto "foo"
    }

    #[test]
    fn b_unicode() {
        let mut s = state("héllo wörld");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 9); // inside "wörld"
        handle_key(key('B'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 6)); // start of "wörld"
    }

    #[test]
    fn caret_first_non_blank() {
        let mut s = state("   foo");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 5);
        handle_key(key('^'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 3));
    }

    #[test]
    fn caret_all_whitespace_row() {
        let mut s = state("    ");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 2);
        handle_key(key('^'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 0));
    }

    #[test]
    fn f_found_and_not_found() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        assert!(handle_key(key('f'), &mut s, &mut e)); // pending
        assert!(handle_key(key('b'), &mut s, &mut e)); // resolve
        assert_eq!(at(&s), (0, 4)); // the 'b' in "bar"

        // Not found leaves the cursor unchanged.
        handle_key(key('f'), &mut s, &mut e);
        handle_key(key('z'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 4));
    }

    #[test]
    fn capital_f_backward() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        s.cursor = Index2::new(0, 6); // the last 'r'
        handle_key(key('F'), &mut s, &mut e);
        handle_key(key('o'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 2)); // last 'o' before the cursor
    }

    #[test]
    fn t_and_capital_t_offsets() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        handle_key(key('t'), &mut s, &mut e);
        handle_key(key('b'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 3)); // one short of the 'b'

        s.cursor = Index2::new(0, 6);
        handle_key(key('T'), &mut s, &mut e);
        handle_key(key('f'), &mut s, &mut e);
        assert_eq!(at(&s), (0, 1)); // one short (after) the 'f'
    }

    #[test]
    fn pending_aborted_by_esc() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        handle_key(key('f'), &mut s, &mut e);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_key(esc, &mut s, &mut e)); // consumed, aborts
        assert_eq!(at(&s), (0, 0));
        // The find is gone: a following char is not treated as a target.
        assert!(!handle_key(key('x'), &mut s, &mut e));
    }

    #[test]
    fn pending_aborted_by_non_char_key() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        handle_key(key('f'), &mut s, &mut e);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(handle_key(down, &mut s, &mut e)); // consumed, aborts
        assert_eq!(at(&s), (0, 0));
    }

    #[test]
    fn modified_char_never_arms_or_resolves_a_find() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        // Ctrl-f is not a motion starter — falls through unconsumed.
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(!handle_key(ctrl_f, &mut s, &mut e));
        // A pending find aborts on a Ctrl-modified char instead of treating it
        // as the target (Ctrl-s must not become a search for 's').
        handle_key(key('f'), &mut s, &mut e);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(handle_key(ctrl_s, &mut s, &mut e)); // consumed, aborts
        assert_eq!(at(&s), (0, 0));
        assert!(!handle_key(key('x'), &mut s, &mut e)); // pending gone
    }

    #[test]
    fn shifted_char_still_works() {
        // Terminals report 'W' as Char('W') + SHIFT; SHIFT must not disqualify.
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        let shift_w = KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT);
        assert!(handle_key(shift_w, &mut s, &mut e));
        assert_eq!(at(&s), (0, 4));
    }

    #[test]
    fn unhandled_key_returns_false() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        assert!(!handle_key(key('x'), &mut s, &mut e));
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(!handle_key(down, &mut s, &mut e));
    }

    #[test]
    fn reset_clears_pending() {
        let mut s = state("foo bar");
        let mut e = VimExt::default();
        handle_key(key('f'), &mut s, &mut e);
        e.reset();
        // Pending gone: next char is not consumed as a target.
        assert!(!handle_key(key('b'), &mut s, &mut e));
    }
}
