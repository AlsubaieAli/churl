//! Workspace/explorer plumbing extracted from `app.rs` (M7.11): the tree-reload
//! and buffer-remap seam plus the guarded endpoint/workspace switch. Grandchild
//! module of `app`, so `impl App` here keeps full access to `App`'s private
//! fields and methods without any visibility widening — see DECISIONS.md,
//! "Module boundaries". Every method carries `pub(in crate::tui::app)` (its exact
//! original scope) because it is called from the parent module and/or sibling
//! handler clusters.

use super::super::*;

impl App {
    /// Rebuilds the explorer tree from disk, preserving expansion + cursor as best
    /// it can (cursor clamps to the new row count), then re-derives each buffer's
    /// indices from its file path (see [`App::remap_buffers`]).
    pub(in crate::tui::app) fn reload_explorer(&mut self) -> Result<()> {
        self.explorer.reload(self.workspace.as_ref())?;
        self.remap_buffers();
        self.surface_explorer_warnings();
        // A reload can empty the sequence list (e.g. every sequence deleted on
        // disk); reconcile the left-column focus so it never strands on an empty
        // sub-pane.
        self.enforce_left_active_invariant();
        Ok(())
    }

    /// Re-derives each buffer's explorer indices from its file path after a tree
    /// reload. Collections are name-sorted, so creating, renaming, or deleting
    /// *another* collection shifts indices — a stale `endpoint.collection` would
    /// silently read the wrong collection's `folder.toml` vars at send time. A
    /// buffer whose file vanished is removed (the post-delete case); `active` is
    /// clamped so it still points at a live buffer.
    pub(in crate::tui::app) fn remap_buffers(&mut self) {
        let mut i = 0;
        while i < self.buffers.len() {
            let file = self.buffers[i].file().to_path_buf();
            if !file.exists() {
                self.buffers.remove(i);
                // A removal before/at `active` shifts it left (never below 0).
                if self.active > i || self.active >= self.buffers.len() {
                    self.active = self.active.saturating_sub(1);
                }
                continue;
            }
            if let Some(ci) = self.explorer.collection_index_for_file(&file)
                && let Some(b) = self.buffers[i].as_endpoint_mut()
            {
                b.endpoint.collection = ci;
            }
            i += 1;
        }
    }

    /// Moves the explorer cursor onto the endpoint at `file` (expanding its
    /// collection) and loads it into a buffer.
    pub(in crate::tui::app) fn select_endpoint_file(&mut self, file: &Path) -> Result<()> {
        if let Some(selected) = self.explorer.select_file(file)? {
            self.open_or_focus_buffer(selected);
        }
        Ok(())
    }

    /// The endpoint-switch seam: every path that opens/focuses an endpoint or
    /// switches workspace goes through here. Multi-buffer removed the
    /// cross-endpoint discard guard — opening endpoint Y no longer destroys X
    /// (each keeps its own buffer), so `Row`/`File` targets open-or-focus with NO
    /// confirm. Only a `Workspace` switch destroys every buffer, so a dirty
    /// workspace switch still defers behind a single
    /// [`ConfirmPurpose::DiscardChanges`] (discard-all) overlay, target parked in
    /// `pending_load`.
    pub(in crate::tui::app) fn guarded_load(&mut self, target: PendingLoad) -> Result<()> {
        // The guard fires when ANY buffer is dirty (not just the active one) — a
        // switch destroys every buffer, so a non-active dirty buffer must still
        // prompt (else its unsaved edits vanish silently).
        if matches!(target, PendingLoad::Workspace(_)) && self.any_buffer_dirty() {
            self.pending_load = Some(target);
            self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
            return Ok(());
        }
        self.perform_load(target)
    }

    /// Performs a (possibly previously deferred) endpoint load.
    pub(in crate::tui::app) fn perform_load(&mut self, target: PendingLoad) -> Result<()> {
        match target {
            PendingLoad::Row(row) => {
                self.explorer.cursor = row;
                if let Some(selected) = self.explorer.select()? {
                    self.open_or_focus_buffer(selected);
                }
            }
            PendingLoad::File(file) => self.select_endpoint_file(&file)?,
            PendingLoad::Workspace(path) => self.switch_workspace(path)?,
        }
        Ok(())
    }

    /// Switches the whole app to the workspace rooted at `path` (quick-jump
    /// workspace picker). Opens the new manifest, rebuilds the explorer, and
    /// resets every endpoint/workspace-scoped field so nothing from the old
    /// workspace leaks in. On a failed open, the current state is left intact and
    /// the error is surfaced (fail loudly, never wipe on failure).
    pub(in crate::tui::app) fn switch_workspace(&mut self, path: PathBuf) -> Result<()> {
        let new_ws = match OpenWorkspace::open(&path) {
            Ok(ws) => ws,
            Err(err) => {
                self.notify(format!("failed to open workspace: {err}"));
                return Ok(());
            }
        };
        let name = new_ws.manifest().name.clone();

        // Abort any in-flight request from the old workspace's buffers (their
        // responses are no longer relevant); dropping the handle also drops the
        // stale generation.
        for buf in &mut self.buffers {
            if let Some(in_flight) = buf.as_endpoint_mut().and_then(|b| b.in_flight.take()) {
                in_flight.handle.abort();
            }
        }

        self.workspace = Some(new_ws);
        self.explorer.reload(self.workspace.as_ref())?;
        self.explorer.cursor = 0;

        // Drop every buffer — all endpoint/response/dirty/editor state lives
        // inside them, so a clear resets it all in one move (nothing from the old
        // workspace leaks in).
        self.buffers.clear();
        self.active = 0;
        // The active profile is defined per-workspace; a stale name could
        // accidentally resolve against the new workspace's profiles.
        self.active_profile = None;
        self.pending_load = None;
        self.zoom = None;
        // Clean slate for the sequences sub-pane: the new workspace's list is
        // unrelated to the old one. Reset to endpoints-zoomed — a fresh workspace
        // always lands on the endpoints tree regardless of its sequence count.
        self.left_active = LeftPane::Endpoints;
        self.focus_before_explorer = None;
        // set_focus(Explorer) also un-hides the explorer if it was hidden.
        self.set_focus(Pane::Explorer);

        // Record the switch in the recency table (canonical path, deduped).
        if let Some(store) = self.history.as_ref() {
            let canonical = canonical_path(&path);
            let _ = store.touch_workspace(&canonical.to_string_lossy(), now_ms());
        }
        self.notify(format!("switched to {name}"));
        Ok(())
    }
}
