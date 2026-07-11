//! Buffer/tab management handlers extracted from `app.rs`.
//! Grandchild module of `app`, so `impl App` here keeps full access to `App`'s
//! private fields and methods without any visibility widening — see
//! DECISIONS.md, "Module boundaries".

use super::super::*;

impl App {
    /// Focuses buffer `idx`, evicting the *previously* active buffer's highlight
    /// cache when leaving it. Only the active buffer enqueues highlight
    /// jobs, so an inactive buffer's cache is dead weight that would otherwise
    /// accumulate across every open buffer — bounding total highlight-cache
    /// memory to (active buffers × 64) rather than (all buffers × 64). The
    /// pending-highlight guard is cleared too so re-focusing re-enqueues cleanly.
    /// A same-index focus is a no-op (never evicts the buffer you're staying on).
    pub(in crate::tui::app) fn set_active_buffer(&mut self, idx: usize) {
        if idx == self.active {
            return;
        }
        if let Some(b) = self
            .buffers
            .get_mut(self.active)
            .and_then(Buffer::as_endpoint_mut)
        {
            b.highlight_cache.clear();
            b.pending_highlight = None;
        }
        self.active = idx;
    }

    /// Dedup-or-push: focus an already-open buffer for the same endpoint file
    /// (keeping its edits/response), else push a fresh buffer. Never replaces
    /// another buffer — each endpoint keeps its own edit/response/dirty state.
    pub(in crate::tui::app) fn open_or_focus_buffer(&mut self, selected: SelectedEndpoint) {
        if let Some(i) = self.buffer_index_for_path(&selected.file) {
            self.set_active_buffer(i);
            return;
        }
        self.buffers.push(Buffer::endpoint(selected));
        self.set_active_buffer(self.buffers.len() - 1);
    }

    /// Cycles to the next (`forward`) or previous buffer, wrapping. No-op when empty.
    pub(in crate::tui::app) fn buffer_cycle(&mut self, forward: bool) {
        let len = self.buffers.len();
        if len == 0 {
            return;
        }
        let next = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        self.set_active_buffer(next);
    }

    /// Jumps directly to the `n`th open buffer/tab (1-based; `<leader>t <n>`).
    /// Reuses the same `self.active` focus mechanism `buffer_cycle`
    /// drives — no duplicated focus logic. Out of range (`n` > open count, incl.
    /// `n == 0`) is a graceful no-op with a brief status message; never a panic
    /// or a wrong-tab jump.
    pub(in crate::tui::app) fn focus_buffer_index(&mut self, n: usize) {
        let len = self.buffers.len();
        if n == 0 || n > len {
            self.notify(format!("no tab {n}"));
            return;
        }
        self.set_active_buffer(n - 1);
    }

    /// The active buffer index that should be selected after the buffer at
    /// `closed` is removed. Empty → `0`; a close before `active` shifts it left;
    /// closing the active buffer picks its right neighbour (clamped to the new
    /// last); a close after `active` leaves it unchanged. Assumes `closed` has
    /// already been removed, i.e. `self.buffers.len()` is the post-removal len.
    pub(in crate::tui::app) fn new_active_after_close(&self, closed: usize) -> usize {
        let len = self.buffers.len();
        if len == 0 {
            return 0;
        }
        if closed < self.active {
            self.active - 1
        } else if closed == self.active {
            closed.min(len - 1)
        } else {
            self.active
        }
    }

    /// Closes the buffer at `i`. A dirty buffer defers behind a
    /// [`ConfirmPurpose::DiscardChanges`] prompt (its path parked in
    /// `pending_close`); a clean buffer aborts any in-flight request (silently —
    /// no cancelled history row) and is removed immediately, with `active`
    /// clamped via [`App::new_active_after_close`]. No-op on an out-of-range `i`.
    pub(in crate::tui::app) fn close_buffer(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        if self.buffers[i].is_dirty() {
            let path = self.buffers[i].file().to_path_buf();
            self.pending_close = Some(PendingClose::One(path));
            self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
            return;
        }
        self.remove_buffer_at(i);
    }

    /// Removes the buffer at `i` (already known clean or discarded), aborting its
    /// in-flight request first, and clamps `active`. Callers guarantee `i` is in
    /// range.
    pub(in crate::tui::app) fn remove_buffer_at(&mut self, i: usize) {
        if let Some(in_flight) = self.buffers[i]
            .as_endpoint_mut()
            .and_then(|b| b.in_flight.take())
        {
            in_flight.handle.abort();
        }
        self.buffers.remove(i);
        self.active = self.new_active_after_close(i);
    }

    /// Closes every buffer. Clean buffers close immediately; dirty buffers enter
    /// a one-at-a-time discard-confirm queue (keyed by path so removals don't
    /// shift the queue). When no dirty buffer remains the list is cleared.
    pub(in crate::tui::app) fn close_all_buffers(&mut self) {
        let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
        let mut i = 0;
        while i < self.buffers.len() {
            if self.buffers[i].is_dirty() {
                queue.push_back(self.buffers[i].file().to_path_buf());
                i += 1;
            } else {
                self.remove_buffer_at(i);
                // remove shifts everything after `i` left — revisit the same index.
            }
        }
        if queue.is_empty() {
            self.buffers.clear();
            self.active = 0;
            return;
        }
        self.pending_close = Some(PendingClose::All(queue));
        self.prompt_next_close_in_queue();
    }

    /// Advances the close-all queue: skips paths whose buffer is already gone,
    /// and opens the discard-confirm for the next still-dirty buffer. When the
    /// queue drains, clears any remaining (all-clean) buffers and returns to
    /// Normal.
    pub(in crate::tui::app) fn prompt_next_close_in_queue(&mut self) {
        loop {
            let front = match self.pending_close.as_ref() {
                Some(PendingClose::All(q)) => q.front().cloned(),
                _ => return,
            };
            let Some(front) = front else { break };
            match self.buffer_index_for_path(&front) {
                Some(idx) if self.buffers[idx].is_dirty() => {
                    self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
                    return;
                }
                // Front is gone or no longer dirty — drop it and continue.
                _ => {
                    if let Some(PendingClose::All(q)) = self.pending_close.as_mut() {
                        q.pop_front();
                    }
                }
            }
        }
        // Queue drained: clear whatever clean buffers remain.
        self.pending_close = None;
        self.buffers.clear();
        self.active = 0;
        self.mode = Mode::Normal;
    }

    /// Whether the active endpoint (incl. the edtui body) differs from its
    /// pristine snapshot. Derived — no dirty flag to keep in sync. `false` when
    /// nothing is loaded.
    pub(in crate::tui::app) fn is_dirty(&self) -> bool {
        self.active_buffer().is_some_and(Buffer::is_dirty)
    }

    /// Whether ANY open buffer has unsaved changes (not just the active one).
    /// Drives the workspace-switch guard + its save-all resolution, so a
    /// non-active dirty buffer still prompts and is never silently destroyed.
    pub(in crate::tui::app) fn any_buffer_dirty(&self) -> bool {
        self.buffers.iter().any(Buffer::is_dirty)
    }

    /// Saves EVERY dirty buffer through the normal save path (each buffer becomes
    /// active in turn so `save_request` operates on it). Restores the original
    /// active index afterward. A refused save (e.g. literal-secret auth) leaves
    /// its buffer dirty and surfaces the error — callers gate the switch on
    /// [`App::any_buffer_dirty`] being clear afterward. No-op when nothing dirty.
    pub(in crate::tui::app) fn save_all_dirty_buffers(&mut self) {
        let restore = self.active;
        let mut i = 0;
        while i < self.buffers.len() {
            if self.buffers[i].is_dirty() {
                self.active = i;
                self.save_request();
            }
            i += 1;
        }
        // Clamp the restore target in case the buffer list shrank (it never does
        // here, but stay defensive).
        self.active = restore.min(self.buffers.len().saturating_sub(1));
    }
}
