//! Body-tab BROWSE-mode key routing (M8.6.1): the full response-parity key set
//! (scroll/copy/search/wrap/pretty/fold/structural-jump/h-pan) plus the two
//! edit-enter paths (`i`/`a` → edtui insert, `Enter` → edtui Normal). Grandchild
//! module of `app` (see `handlers/mod.rs`'s doc) — full access to `App`'s
//! private fields/methods without visibility widening.
//!
//! The response-parity actions themselves are NOT reimplemented here: every
//! arm below calls the exact same `response_*` handler `response.rs` already
//! defines for the Response pane, which operates on whichever surface
//! `active_response`/`active_response_geometry` resolve to (see
//! `ResponseSurface::RequestBody`) — this file is only the Body-tab's
//! browse-key → handler routing table.
//!
//! **Locked-design collision note**: the design lists `i`/`a`/`o` together as
//! the edit-enter keys AND separately lists `o`/`O` as the browse fold keys —
//! `o` cannot mean both "enter insert" and "toggle fold" on the same keypress.
//! Resolved here in favour of **fold** (`o`/`O` match the Response pane
//! exactly, "full response-parity browse" is called out twice and by name in
//! the design; the edit-enter list is satisfied by `i`/`a` alone — vim itself
//! treats `i`/`a` as the core insert-entry pair and `o` as a distinct
//! open-a-new-line motion, not a required third entry point). Flagged for the
//! adversarial review gate rather than silently picked.
use super::super::*;

impl App {
    /// Handles one key while the Body tab is BROWSING (see
    /// [`App::body_browse_active`]). Returns whether the key was consumed
    /// (claimed here, before leader/keymap) — an unclaimed key (`]`/`[`/
    /// digits/`b`/leader/…) falls through to the normal routing in
    /// `handle_normal_key`, so tab-switching, the now request-pane-scoped
    /// body-type picker (`b`), and the leader all keep working while browsing.
    pub(in crate::tui::app) fn body_browse_handle_key(&mut self, key: KeyEvent) -> bool {
        self.ensure_body_browse_built();
        // Enter / Left / Right: non-`Char` keys, matched directly (bare, no
        // modifiers — a Ctrl/Alt-modified arrow is not one of ours).
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => {
                self.enter_body_edit(EditorMode::Normal);
                return true;
            }
            KeyCode::Left if key.modifiers.is_empty() => {
                self.response_scroll_h(false);
                return true;
            }
            KeyCode::Right if key.modifiers.is_empty() => {
                self.response_scroll_h(true);
                return true;
            }
            _ => {}
        }
        // Only a bare (or shifted) char is meaningful browse input beyond the
        // keys above — mirrors `vim_ext::handle_key`'s plain-char extraction, so
        // a Ctrl/Alt-modified key (e.g. the `Ctrl-d`/`Ctrl-u` half-page keys,
        // handled separately via `Action::HalfPageDown/Up` — see `dispatch`) is
        // never misread as one of these letters.
        let plain_char = match key.code {
            KeyCode::Char(c) if (key.modifiers - KeyModifiers::SHIFT).is_empty() => Some(c),
            _ => None,
        };
        let Some(c) = plain_char else {
            return false;
        };
        match c {
            // Edit-enter: `i`/`a` → edtui already in INSERT mode (vim-faithful:
            // from a read view, `i`/`a` start typing). `o` is deliberately NOT
            // claimed here — see the module doc's collision note.
            'i' | 'a' => self.enter_body_edit(EditorMode::Insert),
            'y' => self.response_copy_view(),
            'Y' => self.response_copy_line(),
            // `S` reaches the same save-response-body handler as the Response
            // pane; this surface's view is built via `build_over_text` (an
            // empty `raw_bytes`), so `begin_save_response_body`'s emptiness
            // guard fires here rather than saving the request body — there is
            // no *response* to save while browsing.
            'S' => self.begin_save_response_body(),
            '/' => self.open_body_search(),
            'n' => self.response_search_step(true),
            'N' => self.response_search_step(false),
            'W' => self.response_toggle_wrap(),
            'p' => self.response_toggle_pretty(),
            'o' => self.response_toggle_fold(),
            'O' => self.response_toggle_all_folds(),
            'J' => self.response_structural_jump(true),
            'K' => self.response_structural_jump(false),
            'H' => self.response_scroll_h(false),
            'L' => self.response_scroll_h(true),
            _ => return false,
        }
        true
    }
}
