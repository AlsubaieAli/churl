//! The TUI render layer: the top-level `render` entry point plus its draw-only
//! helpers (`leader_popup`, `render_collapsed_stub`, `prompt_hint`,
//! `confirm_text`, `leader_popup_entries`). Split out of `app.rs` (M7.11) into
//! this child module of `app` so it keeps full access to `App`'s private fields
//! and methods without any visibility widening. Rendering is pure (no I/O) and
//! deterministic, so the `tui_snapshot` snapshots stay byte-identical.

use super::*;

/// Renders the whole UI:
/// - Explorer (left column)
/// - Column B (right): URL bar (slim, display-only) / Request (top half) / Response (bottom half)
/// - Status bar (bottom, 1 line)
/// - Any open overlay
///
/// Pure (no I/O) and deterministic — `TestBackend` snapshots stay stable.
pub fn render(frame: &mut Frame, app: &mut App) {
    // The dedicated message row (deliverable 9) sits above the statusline and
    // only occupies a row while a message is live, so the statusline never moves.
    // The body-search input takes that row when open (vim-style `/query`),
    // shadowing any transient message.
    let body_search_input: Option<String> = (app.mode == Mode::BodySearch).then(|| {
        let q = app.body_search_editor.text();
        let matches = match app.active_response() {
            ResponseState::Done { view } => view.search().map(|s| s.count()).unwrap_or(0),
            _ => 0,
        };
        if q.is_empty() {
            "/".to_owned()
        } else if matches == 0 {
            format!("/{q}    no matches")
        } else {
            format!("/{q}    {matches} matches")
        }
    });
    let message_text = body_search_input
        .clone()
        .or_else(|| app.message.as_ref().map(|m| m.text.clone()));
    let (main, msg_area, status) = if message_text.is_some() {
        let [main, msg, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        (main, Some(msg), status)
    } else {
        let [main, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        (main, None, status)
    };
    // Two-column layout: Explorer left, column B right. The explorer is a
    // *narrow* column (owner prompt): fixed 30 cols — Min(24)+Fill would grow
    // the explorer to half the screen (ratatui distributes excess into Min).
    // When hidden (deliverable 5) the right column takes the full width.
    let (explorer_area, right_area) = if app.explorer_hidden {
        (None, main)
    } else {
        let [explorer_area, right_area] =
            Layout::horizontal([Constraint::Length(30), Constraint::Fill(1)]).areas(main);
        (Some(explorer_area), right_area)
    };
    // Column B split into three rows: URL bar / Request / Response.
    // Zoom (deliverable 4) collapses the unfocused pane to a bordered stub
    // (border + summary + border), keeping its title and tab-bar/stats visible.
    const COLLAPSED_HEIGHT: u16 = 3;
    // The tab strip occupies a single row at the very top of column B, ONLY when
    // at least one buffer is open. `Length(0)` renders nothing so the zero-buffer
    // layout stays byte-identical to the pre-tabs one; a strip shifts the URL
    // bar / request / response down by exactly one row.
    let strip_h: u16 = u16::from(!app.buffers.is_empty());
    let remaining = right_area.height.saturating_sub(urlbar::HEIGHT + strip_h);
    let (req_height, resp_height) = match app.zoom {
        Some(ZoomPane::Request) => (remaining.saturating_sub(COLLAPSED_HEIGHT), COLLAPSED_HEIGHT),
        Some(ZoomPane::Response) => (COLLAPSED_HEIGHT, remaining.saturating_sub(COLLAPSED_HEIGHT)),
        None => {
            let req = remaining / 2;
            (req, remaining - req)
        }
    };
    let [strip_area, urlbar_area, request_area, response_area] = Layout::vertical([
        Constraint::Length(strip_h),
        Constraint::Length(urlbar::HEIGHT),
        Constraint::Length(req_height),
        Constraint::Length(resp_height),
    ])
    .areas(right_area);

    let has_ws = app.workspace.is_some();
    let explorer_focused = app.focus == Pane::Explorer && app.mode == Mode::Normal;
    let theme = app.theme.clone();
    let dirty = app.is_dirty();
    // Every open dirty buffer's file — each such explorer row gets the accent
    // ● suffix (matched by path in the explorer render), and the active buffer's
    // dirtiness drives the URL-bar dot below.
    let dirty_files: Vec<std::path::PathBuf> = app
        .buffers
        .iter()
        .filter(|b| b.is_dirty())
        .map(|b| b.file().to_path_buf())
        .collect();
    if let Some(explorer_area) = explorer_area {
        // The left column always splits into endpoints + sequences (M7.10 stage
        // B — the sequences sub-pane is peek-symmetric, always present). The
        // focused sub-pane (`left_active`) gets Fill(1); the other collapses to
        // a 3-row stub. Endpoints on top, sequences on the bottom.
        let seq_focused = explorer_focused && app.left_active == LeftPane::Sequences;
        let tree_focused = explorer_focused && app.left_active == LeftPane::Endpoints;
        // In jump-mode the endpoints/sequences regions carry their `e`/`s`
        // mnemonics on whichever face is drawn (full-height or collapsed stub).
        let explorer_jump_label = app
            .jump
            .as_ref()
            .and_then(|j| j.label_for_pane(Pane::Explorer));
        let seq_jump_label = app.jump.as_ref().and_then(|j| j.label_for_sequences());
        let (tree_area, seq_area) = if app.left_active == LeftPane::Sequences {
            let [tree, seq] =
                Layout::vertical([Constraint::Length(COLLAPSED_HEIGHT), Constraint::Fill(1)])
                    .areas(explorer_area);
            (tree, seq)
        } else {
            let [tree, seq] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(COLLAPSED_HEIGHT)])
                    .areas(explorer_area);
            (tree, seq)
        };
        if app.left_active == LeftPane::Endpoints {
            explorer::render(
                frame,
                tree_area,
                &mut app.explorer,
                tree_focused,
                has_ws,
                &theme,
                app.jump.as_ref(),
                &dirty_files,
            );
        } else {
            // Endpoints collapsed to a stub summarizing the current selection.
            let summary = app
                .explorer
                .selected_name()
                .map(|name| Line::from(ratatui::text::Span::styled(name, theme.statusline)))
                .unwrap_or_else(|| Line::from(""));
            render_collapsed_stub(
                frame,
                tree_area,
                "Explorer",
                explorer_jump_label,
                summary,
                &theme,
            );
        }
        if app.left_active == LeftPane::Sequences {
            explorer::render_sequences_pane(
                frame,
                seq_area,
                &mut app.explorer,
                seq_focused,
                &theme,
                seq_jump_label,
            );
        } else {
            let summary = explorer::sequences_stub_summary(&app.explorer, &theme);
            render_collapsed_stub(
                frame,
                seq_area,
                "Sequences",
                seq_jump_label,
                summary,
                &theme,
            );
        }
    }
    // The tab strip (top of column B), rendered only when a buffer is open.
    if strip_h > 0 {
        let tab_items: Vec<tab_strip::TabItem> = app
            .buffers
            .iter()
            .map(|b| tab_strip::TabItem {
                short_name: b
                    .as_endpoint()
                    .map(|e| e.endpoint.endpoint.name.clone())
                    .unwrap_or_default(),
                dirty: b.is_dirty(),
            })
            .collect();
        tab_strip::render(frame, strip_area, &tab_items, app.active, &theme);
    }

    // Bind the active buffer by *field* access (not the `&mut self` accessor) so
    // `app.jump`/`focus`/`theme`/`highlight_tx`/… stay independently borrowable
    // while we hold the buffer mutably (the render borrow-split). The
    // request/editor/tabs/response/cache are disjoint fields, so they co-borrow
    // cleanly. With no buffer loaded we render fresh defaults — byte-identical to
    // the pre-refactor flat fields, always default with nothing loaded.
    let active = app.active;
    // No-buffer response fallback. In production `orphan_response` is always Idle
    // (a response requires a loaded endpoint); the response-pane isolation
    // snapshots set it to render a response with no endpoint. Bound before `buf`
    // (disjoint field) so both borrows coexist.
    let default_response: &ResponseState = &app.orphan_response;
    let buf = app
        .buffers
        .get_mut(active)
        .and_then(Buffer::as_endpoint_mut);
    let mut default_editor = EditorState::default();
    let mut default_tabs = RequestTabs::default();
    let default_cache: HashMap<u64, Vec<Line<'static>>> = HashMap::new();

    // Split the buffer into the disjoint pieces the render fns take.
    let (selected_request, editor, tabs, response, cache, url_editor, resp_scroll, resp_cursor) =
        match buf {
            Some(b) => (
                Some(&b.endpoint.endpoint.request),
                &mut b.editor,
                &mut b.tabs,
                &b.response,
                &b.highlight_cache,
                b.url_editor.as_mut(),
                b.geometry.scroll,
                b.geometry.cursor,
            ),
            None => (
                None,
                &mut default_editor,
                &mut default_tabs,
                default_response,
                &default_cache,
                None,
                0,
                0,
            ),
        };
    let req_focused = app.focus == Pane::Request && app.mode == Mode::Normal;
    let resp_focused =
        app.focus == Pane::Response && (app.mode == Mode::Normal || app.mode == Mode::BodySearch);
    let jump = app.jump.as_ref();
    let tick_count = app.tick_count;

    urlbar::render(
        frame,
        urlbar_area,
        urlbar::UrlBarCtx {
            request: selected_request,
            focused: app.focus == Pane::UrlBar && app.mode == Mode::Normal,
            editor: url_editor,
            dirty,
            jump_label: jump.and_then(|j| j.label_for_pane(Pane::UrlBar)),
        },
        &theme,
    );
    // Captured from `response::render`'s outcome so the buffer's response
    // geometry + highlight guard can be written back after the buffer borrow.
    let mut resp_outcome: Option<response::RenderOutcome> = None;
    match app.zoom {
        Some(ZoomPane::Request) => {
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor,
                    tabs,
                    focused: req_focused,
                    theme: &theme,
                    jump,
                },
            );
            let summary = response::collapsed_summary(response, &theme);
            render_collapsed_stub(
                frame,
                response_area,
                "Response",
                jump.and_then(|j| j.label_for_pane(Pane::Response)),
                summary,
                &theme,
            );
        }
        Some(ZoomPane::Response) => {
            let summary = request::collapsed_summary(selected_request, tabs, &theme);
            render_collapsed_stub(
                frame,
                request_area,
                "Request",
                jump.and_then(|j| j.label_for_pane(Pane::Request)),
                summary,
                &theme,
            );
            resp_outcome = Some(response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: response,
                    request: selected_request,
                    focused: resp_focused,
                    scroll: resp_scroll,
                    cursor: resp_cursor,
                    cache,
                    theme: &theme,
                    jump_label: jump.and_then(|j| j.label_for_pane(Pane::Response)),
                    tick_count,
                },
            ));
        }
        None => {
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor,
                    tabs,
                    focused: req_focused,
                    theme: &theme,
                    jump,
                },
            );
            resp_outcome = Some(response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: response,
                    request: selected_request,
                    focused: resp_focused,
                    scroll: resp_scroll,
                    cursor: resp_cursor,
                    cache,
                    theme: &theme,
                    jump_label: jump.and_then(|j| j.label_for_pane(Pane::Response)),
                    tick_count,
                },
            ));
        }
    }
    // Write the response outcome (geometry + highlight guard/job) back into the
    // active buffer, and enqueue a highlight job. Done after the borrow split so
    // it targets the same buffer the render read from.
    if let Some(outcome) = resp_outcome
        && let Some(b) = app
            .buffers
            .get_mut(active)
            .and_then(Buffer::as_endpoint_mut)
    {
        b.geometry.apply_render_outcome(&outcome);
        // Write the clamped horizontal scroll back onto the view so an over-pan
        // (past the widest visible line) self-corrects on the next frame (M7.7).
        if let ResponseState::Done { view } = &mut b.response {
            view.set_h_scroll(outcome.clamped_h_scroll);
        }
        if let Some(job) = outcome.job {
            let dup = b.pending_highlight == Some(job.hash);
            if !dup && let Some(tx) = &app.highlight_tx {
                // Mark in-flight only when the send actually succeeded — a dead
                // worker must not wedge the guard.
                let hash = job.hash;
                if tx.send(job).is_ok() {
                    b.pending_highlight = Some(hash);
                }
            }
        }
    }
    // The statusline (deliverable 9) keeps *only* persistent state: focus,
    // endpoint/workspace, profile, dirty, and the in-flight spinner. Transient
    // messages live in the dedicated row below.
    let in_flight = app
        .active_endpoint_buffer()
        .is_some_and(|b| b.in_flight.is_some());
    statusline::render(
        frame,
        status,
        statusline::StatusCtx {
            focus: app.focus.name(),
            workspace: app.workspace.as_ref().map(|ws| ws.manifest().name.as_str()),
            profile: app.active_profile.as_deref(),
            dirty,
            in_flight,
            tick_count: app.tick_count,
            theme: &theme,
        },
    );
    // The dedicated message row, only when a message is live.
    if let (Some(area), Some(text)) = (msg_area, &message_text) {
        message::render(frame, area, text, &theme);
    }

    if let Some(picker_state) = &app.picker {
        picker::render(frame, main, picker_state, &theme);
    }

    // CRUD / method overlays render over the whole main area.
    // While the body-search input is open OVER a runner Response region (note #2),
    // keep drawing that runner (with the `/query` row overlaid) instead of falling
    // through to the main two-column layout — the search targets the runner's
    // response. The overlay render decision uses this effective mode; every other
    // overlay still keys on the live `app.mode`.
    let overlay_mode = if app.mode == Mode::BodySearch {
        app.body_search_return
    } else {
        app.mode
    };
    match overlay_mode {
        Mode::MethodMenu => {
            if let Some(method) = app.live_request().map(|r| r.method) {
                method_menu::render(frame, main, method, &theme);
            }
        }
        Mode::Prompt(purpose) => {
            let hint = prompt_hint(app, purpose);
            prompt::render_prompt(
                frame,
                main,
                purpose.title(),
                &app.prompt_editor,
                hint.as_deref(),
                &theme,
            );
        }
        Mode::Confirm(purpose) => {
            let (title, question, hint) = confirm_text(purpose, app.pending_close.is_some());
            prompt::render_confirm(frame, main, title, question, hint, &theme);
        }
        Mode::EnvEditor => {
            if let Some(editor) = &app.env_editor {
                env_editor::render(frame, main, editor, &theme);
            }
        }
        Mode::Sequence => match app.sequence_view {
            SeqView::Edit => {
                if let Some(editor) = &app.sequence_editor {
                    sequence_editor::render(frame, main, editor, &theme);
                }
            }
            SeqView::Run => {
                let tick = app.tick_count;
                let active = app.active;
                // These overlays share the active buffer's highlight cache/guard
                // (keyed by viewport hash, so no cross-contamination); with no
                // buffer loaded they fall back to a per-frame empty cache — only
                // cross-frame caching is skipped, invisible to snapshots.
                let scratch_cache = HashMap::new();
                let mut scratch_pending = None;
                let cache = app
                    .buffers
                    .get(active)
                    .and_then(Buffer::as_endpoint)
                    .map(|b| &b.highlight_cache)
                    .unwrap_or(&scratch_cache);
                let job = match app.sequence_runner.as_mut() {
                    Some(runner) => {
                        sequence_runner::render(frame, main, runner, tick, cache, &theme)
                    }
                    None => None,
                };
                if let Some(job) = job {
                    let pending = app
                        .buffers
                        .get(active)
                        .and_then(Buffer::as_endpoint)
                        .map(|b| b.pending_highlight)
                        .unwrap_or(scratch_pending);
                    let dup = pending == Some(job.hash);
                    if !dup && let Some(tx) = &app.highlight_tx {
                        let hash = job.hash;
                        if tx.send(job).is_ok() {
                            match app
                                .buffers
                                .get_mut(active)
                                .and_then(Buffer::as_endpoint_mut)
                            {
                                Some(b) => b.pending_highlight = Some(hash),
                                None => scratch_pending = Some(hash),
                            }
                        }
                    }
                }
                let _ = scratch_pending;
            }
        },
        Mode::LoadRunner => {
            let tick = app.tick_count;
            let active = app.active;
            let scratch_cache = HashMap::new();
            let mut scratch_pending = None;
            let cache = app
                .buffers
                .get(active)
                .and_then(Buffer::as_endpoint)
                .map(|b| &b.highlight_cache)
                .unwrap_or(&scratch_cache);
            let job = match app.load_runner.as_mut() {
                Some(runner) => load_runner::render(frame, main, runner, tick, cache, &theme),
                None => None,
            };
            if let Some(job) = job {
                let pending = app
                    .buffers
                    .get(active)
                    .and_then(Buffer::as_endpoint)
                    .map(|b| b.pending_highlight)
                    .unwrap_or(scratch_pending);
                let dup = pending == Some(job.hash);
                if !dup && let Some(tx) = &app.highlight_tx {
                    let hash = job.hash;
                    if tx.send(job).is_ok() {
                        match app
                            .buffers
                            .get_mut(active)
                            .and_then(Buffer::as_endpoint_mut)
                        {
                            Some(b) => b.pending_highlight = Some(hash),
                            None => scratch_pending = Some(hash),
                        }
                    }
                }
            }
            let _ = scratch_pending;
        }
        _ => {}
    }

    // The URL vim-popup editor (deliverable 7) renders over the main area.
    if let Some(b) = app.active_endpoint_buffer_mut()
        && let Some(editor) = b.url_popup.as_mut()
    {
        urlbar::render_popup(frame, main, editor, &theme);
    }

    // The two-level which-key leader popup (deliverable 1).
    if let Some(state) = app.leader.clone() {
        let (title, entries) = leader_popup_entries(app, &state);
        leader_popup::render(frame, main, &title, &entries, &theme);
    }

    // The `?` help overlay (deliverable 8), rendered from the live keymap.
    if app.help_open {
        let outcome = help::render(
            frame,
            main,
            &app.keymap,
            app.help_scroll,
            &theme,
            app.help_search.as_ref(),
        );
        app.help_scroll = app.help_scroll.min(outcome.total.saturating_sub(1));
        app.help_viewport_height = outcome.viewport_height;
    }
}

/// Builds the `(title, entries)` for the which-key popup at `state`. Root shows
/// sorted direct binds plus one `"<key>   ▸ <submenu>"` row per submenu prefix;
/// a submenu shows its own sorted `(combo, label)` rows.
pub(super) fn leader_popup_entries(
    app: &App,
    state: &LeaderState,
) -> (String, Vec<(String, String)>) {
    match state {
        LeaderState::Root => {
            let mut entries: Vec<(String, String)> = app
                .keymap
                .iter_leader_root_acts()
                .map(|(combo, action)| (combo.to_string(), action.label().to_owned()))
                .collect();
            entries.sort();
            // Submenu prefixes render with a ▸ marker, after the direct binds.
            for (key, label) in app.keymap.leader_menu_combos() {
                entries.push((key, format!("▸ {label}")));
            }
            (" leader ".to_owned(), entries)
        }
        LeaderState::Submenu(menu) => {
            let mut entries: Vec<(String, String)> = app
                .keymap
                .iter_submenu(menu)
                .map(|(combo, action)| (combo.to_string(), action.label().to_owned()))
                .collect();
            entries.sort();
            (
                format!(" leader · {} ", app.keymap.submenu_title(menu)),
                entries,
            )
        }
    }
}

/// The which-key leader popup: a small floating panel listing the bound
/// continuations while a leader chord is in progress.
mod leader_popup {
    use ratatui::Frame;
    use ratatui::layout::{Constraint, Flex, Layout, Rect};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

    use crate::tui::theme::Theme;

    /// Renders the popup with `title` and `(keys, label)` continuation entries.
    pub fn render(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        entries: &[(String, String)],
        theme: &Theme,
    ) {
        let width = entries
            .iter()
            .map(|(k, l)| k.len() + l.len() + 5)
            .max()
            .unwrap_or(20)
            .max(title.len() + 4)
            .clamp(20, 50) as u16;
        let height = entries.len() as u16 + 2;
        let [modal] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::End)
            .areas(area);
        let [modal] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::End)
            .areas(modal);
        frame.render_widget(Clear, modal);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(theme.border_focused)
            .title(title.to_owned())
            .title_style(theme.title);
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        let lines: Vec<Line> = entries
            .iter()
            .map(|(keys, label)| {
                Line::from(vec![
                    Span::styled(format!(" {keys} "), theme.jump_label),
                    Span::raw(format!(" {label}")),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Renders a collapsed zoom stub: the pane's unfocused border + title (with its
/// jump label when active) around the one-line tab-bar/stats summary, so the
/// pane keeps its chrome when collapsed rather than vanishing into a bare row.
fn render_collapsed_stub(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    name: &str,
    jump_label: Option<char>,
    summary: Line<'static>,
    theme: &crate::tui::theme::Theme,
) {
    let title = match jump_label {
        Some(label) => format!(" {name} [{label}] "),
        None => format!(" {name} "),
    };
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(theme.border_unfocused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(summary), inner);
}

/// The dim hint line under a prompt (the collection name to type for a
/// typed-confirm delete; none otherwise).
fn prompt_hint(app: &App, purpose: PromptPurpose) -> Option<String> {
    match purpose {
        PromptPurpose::DeleteCollectionConfirm => app
            .explorer
            .selected_name()
            .map(|name| format!("type \"{name}\" to confirm")),
        PromptPurpose::ImportCollection => Some("path to a Postman v2.1 JSON file".to_owned()),
        PromptPurpose::ExportCollection(_) | PromptPurpose::ExportWorkspace(_) => {
            Some("destination path (must stay inside the workspace)".to_owned())
        }
        PromptPurpose::PasteCurl => Some("paste a curl command".to_owned()),
        _ => None,
    }
}

/// The (title, question, key-hint) for a confirmation overlay. `closing`
/// distinguishes the two `DiscardChanges` flows — a buffer close (`<leader>t x`/
/// `X`, `pending_close` set) vs an endpoint/workspace switch (`pending_load`) —
/// so the copy matches the actual action.
fn confirm_text(
    purpose: ConfirmPurpose,
    closing: bool,
) -> (&'static str, &'static str, &'static str) {
    match purpose {
        ConfirmPurpose::DeleteEndpoint => ("Delete endpoint", "Delete this endpoint?", "[y/n]"),
        ConfirmPurpose::DeleteSequence => ("Delete sequence", "Delete this sequence?", "[y/n]"),
        ConfirmPurpose::DiscardChanges if closing => (
            "Unsaved changes",
            "Close without saving?",
            "s save · d discard · esc stay",
        ),
        ConfirmPurpose::DiscardChanges => (
            "Unsaved changes",
            "Save changes before switching?",
            "s save · d discard · esc stay",
        ),
    }
}
