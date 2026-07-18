//! Debug Inspector overlay + session debug-toggle handlers — `<leader>d`
//! opens the per-exchange Inspector (park-and-return, mirrors
//! `open_body_search`); `<leader>D` flips the session debug-capture toggle.
//! Grandchild module of `app`; the `impl App` here keeps full access to
//! `App`'s private fields and methods without visibility widening.

use super::super::*;
use crate::tui::components::inspector::InspectorOutcome;

impl App {
    /// Opens the Inspector overlay (`<leader>d`) over the latest exchange's
    /// captured trace — `None` when no trace exists yet (debug was off at
    /// send time, or nothing has been sent this session); the overlay renders
    /// a placeholder rather than panicking. Parks the current mode into
    /// [`Self::inspector_return`] (mirroring [`Self::open_body_search`]'s
    /// `body_search_return`), so opening the Inspector from inside a runner
    /// restores that exact mode, state intact, on close.
    pub(in crate::tui::app) fn open_inspector(&mut self) {
        // Unbox into the overlay's own copy — `InspectorState` is a single
        // value inside one `Mode` variant, not an enum arm sized against tiny
        // siblings, so it holds the trace directly (no `Box` needed there).
        let state = InspectorState::new(self.latest_trace.as_deref().cloned());
        // `mem::replace` MOVES the current mode into `inspector_return` rather
        // than dropping it — `Mode` is not `Copy`, and a parked runner's state
        // must survive the overlay.
        self.inspector_return = std::mem::replace(&mut self.mode, Mode::Inspector(state));
    }

    /// Closes the Inspector overlay, restoring the parked mode.
    fn close_inspector(&mut self) {
        self.mode = std::mem::replace(&mut self.inspector_return, Mode::Normal);
    }

    /// Routes a key to the open Inspector overlay and acts on its outcome.
    pub(in crate::tui::app) fn handle_inspector_key(&mut self, key: KeyEvent) {
        let Mode::Inspector(state) = &mut self.mode else {
            return;
        };
        match state.handle_key(key) {
            InspectorOutcome::Consumed => {}
            InspectorOutcome::Close => self.close_inspector(),
            InspectorOutcome::CopyCurl => {
                // `masked_curl()` never leaks: it renders from a masked clone
                // (see `DebugTrace::masked_curl`'s docs), never `resolved_raw`
                // directly. `InspectorOutcome::CopyCurl` is only ever emitted
                // when `state.trace` is `Some` (see `InspectorState::handle_key`).
                if let Mode::Inspector(state) = &self.mode
                    && let Some(trace) = state.trace.as_ref()
                {
                    let curl = trace.masked_curl();
                    self.enqueue_clipboard(&curl, "copied resolved curl (masked)");
                }
            }
        }
    }

    /// Flips the session debug-capture toggle (`<leader>D`). Session-only —
    /// mirrors `<leader>k`'s [`Self::toggle_insecure`] (no write-back to
    /// `config.toml`; a future M8.5 settings panel surfaces a persisted
    /// version). Takes effect on the NEXT send; an already-in-flight request
    /// is unaffected. Flipping ON→OFF does not clear [`Self::latest_trace`],
    /// so the Inspector still shows the last captured exchange after debug
    /// goes back off.
    pub(in crate::tui::app) fn toggle_debug(&mut self) {
        self.debug_enabled = !self.debug_enabled;
        self.notify(if self.debug_enabled {
            "debug capture ON — sends now build an inspectable trace"
        } else {
            "debug capture off"
        });
    }
}
