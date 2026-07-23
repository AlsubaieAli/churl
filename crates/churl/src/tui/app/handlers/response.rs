//! Response-viewer handlers — scrolling, view toggles, body search, and copy —
//! extracted from `app.rs`. Grandchild module of `app`; `impl App`
//! here keeps full access to `App`'s private fields and methods without any
//! visibility widening.

use super::super::*;

impl App {
    /// The coarse maximum cursor row (total display rows minus one, as of the
    /// last render) for the *active response surface* (main pane or a focused
    /// runner Response); render clamps further. `0` when nothing is
    /// loaded.
    pub(in crate::tui::app) fn response_max_cursor(&self) -> usize {
        self.active_response_geometry().total_rows.saturating_sub(1)
    }

    /// Moves the response viewer *cursor* for a navigation action, on the active
    /// response surface. Scroll follows the cursor at render time (see
    /// `response::render`). `g`/`G` jump to the first/last visible display row.
    pub(in crate::tui::app) fn response_scroll(&mut self, action: Action) {
        let max = self.response_max_cursor();
        if !matches!(self.active_response(), ResponseState::Done { .. }) {
            return;
        }
        let g = self.active_response_geometry_mut();
        match action {
            Action::Up => g.cursor = g.cursor.saturating_sub(1),
            Action::Down => g.cursor = (g.cursor + 1).min(max),
            Action::Top => g.cursor = 0,
            Action::Bottom => g.cursor = max,
            _ => {}
        }
    }

    /// Moves the response cursor by half a viewport (scroll follows at render), on
    /// the active response surface.
    pub(in crate::tui::app) fn response_half_page(&mut self, down: bool) {
        let max = self.response_max_cursor();
        if !matches!(self.active_response(), ResponseState::Done { .. }) {
            return;
        }
        let g = self.active_response_geometry_mut();
        let half = (g.viewport_height / 2).max(1);
        g.cursor = if down {
            (g.cursor + half).min(max)
        } else {
            g.cursor.saturating_sub(half)
        };
    }

    /// The live `ResponseView`, when the pane holds a completed response. Reads
    /// the active buffer's response, or the orphan slot when nothing is loaded
    /// (isolation snapshots).
    pub(in crate::tui::app) fn response_view_mut(&mut self) -> Option<&mut ResponseView> {
        match self.active_response_mut() {
            ResponseState::Done { view } => Some(view),
            _ => None,
        }
    }

    /// Resets the active response surface's cursor/scroll (+ optionally the shared
    /// highlight guard/cache) after a view-geometry change. Works on the main pane
    /// or the focused runner Response. The highlight cache/guard is
    /// shared with the active endpoint buffer even for runner responses (they
    /// render through it), so it is cleared via [`Self::clear_active_highlight`].
    pub(in crate::tui::app) fn reset_response_geometry(&mut self, clear_cache: bool) {
        let g = self.active_response_geometry_mut();
        g.cursor = 0;
        g.scroll = 0;
        if clear_cache {
            self.clear_active_highlight();
        } else if let Some(b) = self.active_endpoint_buffer_mut() {
            b.pending_highlight = None;
        }
    }

    /// The logical line under the response cursor (through the last render's
    /// fold/wrap geometry), or `None` when there is no response — on the active
    /// response surface.
    pub(in crate::tui::app) fn response_cursor_logical(&self) -> Option<usize> {
        let g = self.active_response_geometry();
        let (width, cursor) = (g.viewport_width, g.cursor);
        match self.active_response() {
            ResponseState::Done { view } => view.logical_at_display_row(cursor, width),
            _ => None,
        }
    }

    /// `h`: toggle body/headers view. Resets cursor + scroll (the two views have
    /// different geometry) and clears any live search.
    pub(in crate::tui::app) fn response_toggle_headers(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_view_mode();
            self.reset_response_geometry(true);
        }
    }

    /// `W`: toggle soft-wrap. Cursor/scroll geometry changes, so reset them.
    pub(in crate::tui::app) fn response_toggle_wrap(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_wrap();
            self.reset_response_geometry(false);
        }
    }

    /// `H`/`L` (or Left/Right): pan the response horizontal window for unwrapped
    /// long lines. A no-op while wrap is on (the view guards internally) —
    /// wrapped rows already fit the width, so there is nothing to pan. The pan
    /// amount is a fixed column step; render clamps the offset to the widest
    /// visible line and writes the clamped value back, so an over-pan self-corrects.
    pub(in crate::tui::app) fn response_scroll_h(&mut self, right: bool) {
        /// Columns panned per keypress.
        const H_STEP: usize = 8;
        if let Some(view) = self.response_view_mut() {
            view.scroll_h(right, H_STEP);
        }
    }

    /// `p`: toggle raw↔pretty body rendering. Body text/line count change
    /// (and `toggle_pretty` resets folds), so reset cursor/scroll geometry and
    /// clear the highlight cache. Pretty is JSON-only in v1 — no-op with a notice
    /// outside a JSON body view, which would otherwise silently do nothing.
    pub(in crate::tui::app) fn response_toggle_pretty(&mut self) {
        let is_json_body = match self.active_response() {
            ResponseState::Done { view } => {
                view.view_mode() == ViewMode::Body
                    && view.syntax() == crate::tui::highlight::SyntaxToken::Json
            }
            _ => false,
        };
        if !is_json_body {
            self.notify("pretty: JSON body only");
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_pretty();
            self.reset_response_geometry(true);
        }
    }

    /// `s`: toggle A→Z sorting of pretty JSON object keys. Only meaningful
    /// on a pretty JSON body — guard and notify otherwise (mirrors the
    /// pretty-outside-JSON notice). Text/line count change (and `toggle_sort_keys`
    /// resets folds), so reset cursor/scroll and clear the highlight cache.
    pub(in crate::tui::app) fn response_toggle_sort_keys(&mut self) {
        let is_pretty_json_body = match self.active_response() {
            ResponseState::Done { view } => {
                view.view_mode() == ViewMode::Body
                    && view.syntax() == crate::tui::highlight::SyntaxToken::Json
                    && view.pretty()
            }
            _ => false,
        };
        if !is_pretty_json_body {
            self.notify("sort: pretty JSON only");
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_sort_keys();
            self.reset_response_geometry(true);
        }
    }

    /// `#`: toggle the line-number gutter (default on). The
    /// gutter shrinks the effective body width, so wrap boundaries and the total
    /// display-row count can change — reset cursor/scroll geometry (as `W` does).
    /// The displayed text is untouched, so the highlight cache is kept (it is
    /// keyed on `line_numbers`, so the correctly-windowed lines are re-highlighted
    /// on demand); works in any view (body or headers).
    pub(in crate::tui::app) fn response_toggle_line_numbers(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_line_numbers();
            self.reset_response_geometry(false);
        }
    }

    /// Why folding is unsupported right now, or `None` when available. The headers
    /// view of a JSON response gets its own reason — "JSON responses only" would
    /// be wrong there.
    pub(in crate::tui::app) fn fold_unsupported_notice(&self) -> Option<&'static str> {
        let view = match self.active_response() {
            ResponseState::Done { view } => view,
            _ => return Some("folding: no response"),
        };
        if view.view_mode() == ViewMode::Headers {
            return Some("folding: body view only");
        }
        if view.syntax() != crate::tui::highlight::SyntaxToken::Json {
            return Some("folding: JSON responses only");
        }
        None
    }

    /// `o`: fold/unfold the innermost JSON region at the cursor.
    pub(in crate::tui::app) fn response_toggle_fold(&mut self) {
        let Some(logical) = self.response_cursor_logical() else {
            return;
        };
        if let Some(notice) = self.fold_unsupported_notice() {
            self.notify(notice);
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_fold_at(logical);
        }
        self.clear_active_highlight();
    }

    /// `O`: collapse all top-level JSON regions, or expand all. Non-JSON no-ops
    /// with a notice.
    pub(in crate::tui::app) fn response_toggle_all_folds(&mut self) {
        if let Some(notice) = self.fold_unsupported_notice() {
            self.notify(notice);
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_all_folds();
            self.reset_response_geometry(true);
        }
    }

    /// `J`/`K`: jump the response cursor forward/inward or backward/outward to the
    /// next/previous collapsible JSON node, skipping leaf lines and hidden folded
    /// subtrees. JSON body only — the same guard and notice as folding (`o`/`O`),
    /// so a non-JSON or headers view says why instead of silently no-opping.
    pub(in crate::tui::app) fn response_structural_jump(&mut self, forward: bool) {
        let Some(from) = self.response_cursor_logical() else {
            return;
        };
        if let Some(notice) = self.fold_unsupported_notice() {
            self.notify(notice);
            return;
        }
        // Read the surface width immutably before taking the view mutably (cursor
        // and view live on the same surface, so they cannot be borrowed together).
        // Resolve the target row through the view, then write the cursor back.
        let width = self.active_response_geometry().viewport_width;
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let row = view
            .structural_target(from, forward)
            .and_then(|logical| view.display_row_for_logical(logical, width));
        if let Some(row) = row {
            self.active_response_geometry_mut().cursor = row;
        }
    }

    /// `/`: open the incremental body-search input in the message-row position.
    pub(in crate::tui::app) fn open_body_search(&mut self) {
        if self.response_view_mut().is_none() {
            self.notify("no response to search");
            return;
        }
        self.body_search_editor = LineEditor::new("");
        // Seed an empty live search so highlighting/feedback engage immediately.
        if let Some(view) = self.response_view_mut() {
            view.set_search(String::new());
        }
        // Remember where to return: Normal for the main pane, or the live runner
        // mode when search was opened over a runner Response region.
        // `mem::replace` MOVES the current mode (incl. a `LoadRunner(state)`
        // payload) into `body_search_return` rather than copying it — `Mode` is
        // not `Copy`, and the runner state must survive the body-search overlay
        // (it is read back via `load_runner()`, which consults
        // `body_search_return` while `Mode::BodySearch` is active).
        self.body_search_return = std::mem::replace(&mut self.mode, Mode::BodySearch);
    }

    /// Handles one key while the body-search input is open. Every keystroke
    /// recomputes matches and jumps to the first; Enter commits (keeps matches
    /// for `n`/`N`), Esc cancels (clears the search).
    pub(in crate::tui::app) fn handle_body_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Move the parked mode back (restoring any `LoadRunner(state)`),
                // leaving `Normal` behind in `body_search_return`.
                self.mode = std::mem::replace(&mut self.body_search_return, Mode::Normal);
                if let Some(view) = self.response_view_mut() {
                    view.clear_search();
                }
            }
            KeyCode::Enter => {
                self.mode = std::mem::replace(&mut self.body_search_return, Mode::Normal);
                self.response_center_on_match();
            }
            _ => {
                self.body_search_editor.handle_key(key);
                let query = self.body_search_editor.text();
                if let Some(view) = self.response_view_mut() {
                    view.set_search(query);
                }
                self.response_center_on_match();
            }
        }
    }

    /// `n`/`N`: step to the next/previous match (wrapping), scrolling it into
    /// view and auto-unfolding its region.
    pub(in crate::tui::app) fn response_search_step(&mut self, forward: bool) {
        let has_search = self
            .response_view_mut()
            .map(|v| v.search().is_some())
            .unwrap_or(false);
        if !has_search {
            return;
        }
        let stepped = self.response_view_mut().and_then(|v| v.step_match(forward));
        if stepped.is_some() {
            self.clear_active_highlight();
            self.response_center_on_match();
            if let Some(view) = self.response_view_mut()
                && let Some(search) = view.search()
                && let Some(ord) = search.current_ordinal()
            {
                let total = search.count();
                self.notify(format!("match {ord}/{total}"));
            }
        } else {
            self.notify("no matches");
        }
    }

    /// Moves the response cursor onto the current search match's logical line,
    /// so scroll follows it into view at the next render. Also pans the horizontal
    /// window so an unwrapped match that lies past the right edge is brought into
    /// view (horizontal search-into-view); inert while wrap is on.
    pub(in crate::tui::app) fn response_center_on_match(&mut self) {
        // Read the surface's width first (immutable), then take the view mutably —
        // the cursor lives in the geometry, the view in the response, both on the
        // same surface, so they cannot be borrowed together. Compute the target row
        // + pan the window through the view, then write the cursor back.
        let width = self.active_response_geometry().viewport_width;
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let row = view
            .current_match_line()
            .and_then(|logical| view.display_row_for_logical(logical, width));
        if let Some((start, end)) = view.current_match_columns() {
            view.ensure_column_visible(start, end, width);
        }
        if let Some(row) = row {
            self.active_response_geometry_mut().cursor = row;
        }
    }

    /// `y`: copy the current response's full text via OSC 52 (capped).
    ///
    /// On a `Done` row this copies the byte-exact body. On a `Failed` row it
    /// copies an honest error blurb — the error message plus the request
    /// method+URL when known — via the same clipboard path, so `y` is never a
    /// silent no-op on a transport failure. This branch lives in the single
    /// shared copy handler used by the main pane, load runner, and sequence
    /// runner, so all three unified viewers get it at once.
    pub(in crate::tui::app) fn response_copy_view(&mut self) {
        if let Some(view) = self.response_view_mut() {
            let full = view.copy_all().to_owned();
            let truncated = view.truncated();
            self.copy_to_clipboard_view(&full, truncated);
        } else if let Some(text) = self.active_response().failure_copy_text() {
            self.enqueue_clipboard(&text, "copied error");
        } else {
            // No view and no failure blurb (Dropped / Idle / …). Never a silent
            // no-op — say why.
            self.message = Some(Message::new(nothing_to_copy_message(
                self.active_response(),
            )));
        }
    }

    /// `Y`: block-aware copy. On a fold-opener line the whole folded region is
    /// yanked; on any other line, just that logical line. Both go through the
    /// layered clipboard.
    pub(in crate::tui::app) fn response_copy_line(&mut self) {
        // A missing view (Failed / Dropped / Idle) has no line to copy. Give
        // feedback rather than silently doing nothing (drive-test #4a fold-in).
        if self.response_view_mut().is_none() {
            self.message = Some(Message::new(nothing_to_copy_message(
                self.active_response(),
            )));
            return;
        }
        let Some(logical) = self.response_cursor_logical() else {
            return;
        };
        let Some(view) = self.response_view_mut() else {
            return;
        };
        // On a fold opener, copy the whole covered region as one block; otherwise
        // fall back to the single-line copy.
        let (payload, label) = match view.fold_region_at_opener(logical) {
            Some((opener, closer)) => (view.copy_region(opener, closer), "copied block"),
            None => (view.copy_line(logical), "copied line"),
        };
        self.enqueue_clipboard(&payload, label);
    }

    /// Copies the full-view text, reporting its size (with a `(truncated)` note
    /// when the body hit the size cap, and a `copied first X of Y` note when the
    /// 1 MB copy cap kicked in). The message is the *success* message; the run
    /// loop swaps it for "copy failed" if no clipboard path works.
    pub(in crate::tui::app) fn copy_to_clipboard_view(&mut self, text: &str, body_truncated: bool) {
        let full_len = text.len();
        let (payload, queued) = clipboard::cap_payload(text);
        let capped = full_len > queued;
        let mut msg = if capped {
            format!(
                "copied first {} of {}",
                response::fmt_bytes(queued),
                response::fmt_bytes(full_len)
            )
        } else {
            format!("copied {}", response::fmt_bytes(queued))
        };
        // The body-truncation note stacks with the cap note — both facts matter.
        if body_truncated {
            msg.push_str(" (truncated)");
        }
        self.pending_clipboard = Some(PendingCopy {
            payload,
            success_msg: msg,
        });
    }

    /// `S`: opens a typed-path prompt to save the response body to disk,
    /// byte-exact (M8.7) — reads `raw_bytes`, never the pretty-reformatted
    /// display, so the saved file matches the wire regardless of the `p`
    /// toggle. Seeded with a smart default `<endpoint-name>.<ext>` (the
    /// extension sniffed from the response's Content-Type at build time).
    /// Guards loudly rather than opening a prompt with nothing to write: a
    /// bodyless response state (Idle/InFlight/Cancelled/Dropped/Failed), or an
    /// empty view — the Body-tab request-body-browse surface (M8.6.1) has no
    /// *response* to save, only the request body it happens to render through
    /// this same viewer.
    pub(in crate::tui::app) fn begin_save_response_body(&mut self) {
        let Some(view) = self.response_view_mut() else {
            self.message = Some(Message::new(nothing_to_save_message(
                self.active_response(),
            )));
            return;
        };
        if view.raw_bytes().is_empty() {
            self.message = Some(Message::new("nothing to save — empty body"));
            return;
        }
        let ext = view.save_extension();
        let base = self
            .active_endpoint_buffer()
            .map(|b| persistence::slug_of(&b.endpoint.endpoint.name))
            .unwrap_or_else(|| "response".to_owned());
        self.open_prompt(PromptPurpose::SaveResponseBody, &format!("{base}.{ext}"));
    }

    /// Commits the `S` save-response-body prompt: resolves `path_input`
    /// download-style (see [`resolve_save_path`] — cwd-relative, `~`-expanding,
    /// no workspace confinement), creates any missing parent directories, and
    /// writes the response's raw bytes atomically. Re-checks the same
    /// emptiness guard [`Self::begin_save_response_body`] applied (the active
    /// response could in principle have changed between opening the prompt and
    /// committing it — e.g. a `send` landed while the prompt was open). A
    /// truncated body is saved as-is but the success notice says so loudly, per
    /// the locked design — never a silent partial save.
    pub(in crate::tui::app) fn commit_save_response_body(&mut self, path_input: String) {
        let Some(view) = self.response_view_mut() else {
            self.message = Some(Message::new(nothing_to_save_message(
                self.active_response(),
            )));
            return;
        };
        if view.raw_bytes().is_empty() {
            self.message = Some(Message::new("nothing to save — empty body"));
            return;
        }
        let bytes = view.raw_bytes().to_vec();
        let truncated = view.body_truncated();

        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => {
                self.notify(format!(
                    "save failed: cannot determine current directory: {err}"
                ));
                return;
            }
        };
        let home = dirs::home_dir();
        let target = match resolve_save_path(&cwd, home.as_deref(), &path_input) {
            Ok(target) => target,
            Err(err) => {
                self.notify(format!("save failed: {err}"));
                return;
            }
        };
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            self.notify(format!("save failed: {err}"));
            return;
        }
        match persistence::atomic_write(&target, &bytes) {
            Ok(()) => {
                let mut msg = format!("saved {} bytes to {}", bytes.len(), target.display());
                if truncated {
                    msg = format!(
                        "saved {} bytes (response was truncated) to {}",
                        bytes.len(),
                        target.display()
                    );
                }
                self.notify(msg);
            }
            Err(err) => self.notify(format!("save failed: {err}")),
        }
    }
}
