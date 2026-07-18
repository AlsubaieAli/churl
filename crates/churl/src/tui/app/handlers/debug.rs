//! Debug Inspector + Log panel overlay handlers, the session debug-toggle,
//! and the narrow leader-from-runner allowlist — `<leader>d` opens the
//! per-exchange Inspector (park-and-return, mirrors `open_body_search`),
//! `<leader>L` opens the Log panel, `<leader>D` flips the session
//! debug-capture toggle. Grandchild module of `app`; the `impl App` here
//! keeps full access to `App`'s private fields and methods without
//! visibility widening.

use super::super::*;
use crate::tui::components::inspector::InspectorOutcome;
use crate::tui::components::log_panel::LogPanelOutcome;

impl App {
    /// Opens the Inspector overlay (`<leader>d`) over the latest exchange's
    /// captured trace — `None` when no trace exists yet (debug was off at
    /// send time, or nothing has been sent this session); the overlay renders
    /// a placeholder rather than panicking. Parks the current mode into
    /// [`Self::inspector_return`] (mirroring [`Self::open_body_search`]'s
    /// `body_search_return`), so opening the Inspector from inside a runner
    /// restores that exact mode, state intact, on close.
    ///
    /// Also hands the overlay the session Traffic feed (newest first) so
    /// `n`/`N` can browse every captured exchange, not just the latest — but
    /// the DEFAULT view stays [`Self::latest_trace`], not `traffic[0]`: a
    /// debug-off send clears `latest_trace` (see its doc) while Traffic keeps
    /// full history, so defaulting to `traffic[0]` would resurrect a stale
    /// exchange the moment debug goes back off. Browsing history via `n`/`N`
    /// is a deliberate, explicit user action; the default view is not.
    pub(in crate::tui::app) fn open_inspector(&mut self) {
        // Unbox into the overlay's own copy — `InspectorState` is a single
        // value inside one `Mode` variant, not an enum arm sized against tiny
        // siblings, so it holds the trace directly (no `Box` needed there).
        let traffic: Vec<TrafficEntry> = self.traffic.iter().rev().cloned().collect();
        let state = InspectorState::open(self.latest_trace.as_deref().cloned(), traffic);
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
        // The Log ring needs no toggle here — off means the send/load/sequence
        // emit sites simply don't emit (their `if let Some(trace)` guard is
        // `None`), so the ring stops filling; see `log_subscriber`'s docs.
        self.notify(if self.debug_enabled {
            "debug capture ON — sends now build an inspectable trace"
        } else {
            "debug capture off"
        });
    }

    /// Opens the Log panel overlay (`<leader>L`) over the bounded `tracing`
    /// ring. Parks the current mode into [`Self::log_return`], mirroring
    /// [`Self::open_inspector`]. The ring's contents themselves are NOT
    /// snapshotted here — they live on `App::log_ring` and are read fresh
    /// each frame by the render layer, since the ring keeps accumulating
    /// events in the background regardless of whether the panel is open.
    pub(in crate::tui::app) fn open_log_panel(&mut self) {
        self.log_return = std::mem::replace(&mut self.mode, Mode::LogPanel(LogPanelState::new()));
    }

    /// Closes the Log panel overlay, restoring the parked mode.
    fn close_log_panel(&mut self) {
        self.mode = std::mem::replace(&mut self.log_return, Mode::Normal);
    }

    /// Routes a key to the open Log panel overlay and acts on its outcome.
    pub(in crate::tui::app) fn handle_log_panel_key(&mut self, key: KeyEvent) {
        let line_count = self.log_ring.snapshot().len();
        let Mode::LogPanel(state) = &mut self.mode else {
            return;
        };
        match state.handle_key(key, line_count) {
            LogPanelOutcome::Consumed => {}
            LogPanelOutcome::Close => self.close_log_panel(),
        }
    }

    /// Handles the key immediately AFTER the leader key was pressed while a
    /// load/sequence runner mode was active (see
    /// [`Self::runner_leader_pending`]'s doc on `App`). Resolves it against
    /// the leader-ROOT table but acts on ONLY the two debug-overlay entries
    /// (`OpenInspector`/`OpenLogPanel`) — any other bound action, submenu
    /// descent, or unbound key cancels silently, exactly like
    /// [`Self::handle_leader_key`]'s Root arm on an unknown key. This is a
    /// deliberately narrow allowlist, not a general leader-from-runner path:
    /// broadening it would make every leader-root action reachable from
    /// runner mode, which is explicitly out of scope (a separate, larger
    /// latent gap left for a future milestone).
    pub(in crate::tui::app) fn handle_runner_leader_key(&mut self, key: KeyEvent) -> Result<()> {
        self.runner_leader_pending = false;
        match self.keymap.leader_root_lookup(key) {
            Some(LeaderEntry::Act(Action::OpenInspector)) => self.open_inspector(),
            Some(LeaderEntry::Act(Action::OpenLogPanel)) => self.open_log_panel(),
            _ => {}
        }
        Ok(())
    }
}
