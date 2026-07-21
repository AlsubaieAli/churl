//! Row/field/auth/method/url editing handlers — extracted from `app.rs`.
//! Grandchild module of `app`; `impl App` here keeps full
//! access to `App`'s private fields and methods without any visibility
//! widening.

use super::super::*;

impl App {
    /// Handles one key in the URL vim-popup editor. Mode-aware:
    /// - `EditorMode::Search`: everything (incl. Enter/Esc) goes to edtui so
    ///   `/`-search executes (Enter jumps to match → Normal, Esc cancels). Never
    ///   commits from Search mode.
    /// - Otherwise Enter commits (single-logical-line constraint drops any Enter
    ///   edtui would turn into a newline). In Normal mode `vim_ext` motions run
    ///   before the Esc-cancel check, so Esc aborts a pending f/F/t/T find instead
    ///   of closing the popup; then Esc cancels; the rest falls through to edtui.
    ///
    /// Edge: Enter with a find pending still commits (the find drops with the popup).
    pub(in crate::tui::app) fn handle_url_popup_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mode) = self
            .active_endpoint_buffer()
            .and_then(|b| b.url_popup.as_ref().map(|e| e.mode))
        else {
            return Ok(());
        };
        if mode == EditorMode::Search {
            if let Some(b) = self.active_endpoint_buffer_mut()
                && let Some(editor) = b.url_popup.as_mut()
            {
                b.url_popup_events.on_key_event(key, editor);
            }
            return Ok(());
        }
        // Enter commits (single logical line — no newline). Take the popup out
        // first so `commit_url` (which needs `&mut self`) does not overlap the
        // buffer borrow.
        if key.code == KeyCode::Enter {
            let taken = self
                .active_endpoint_buffer_mut()
                .and_then(|b| b.url_popup.take());
            if let Some(editor) = taken {
                let text: String = editor.lines.clone().into();
                // Collapse any stray newlines to enforce a single logical line.
                let text = text.replace(['\n', '\r'], "");
                self.commit_url(text);
            }
            return Ok(());
        }
        // Normal mode: churl-side vim motions win before the Esc-cancel check —
        // Esc while an f/F/t/T find is pending aborts the find (vim), it must
        // not close the popup.
        if mode == EditorMode::Normal
            && let Some(b) = self.active_endpoint_buffer_mut()
            && let Some(editor) = b.url_popup.as_mut()
            && vim_ext::handle_key(key, editor, &mut b.url_popup_vim)
        {
            return Ok(());
        }
        // Esc in Normal mode cancels; in Insert mode edtui uses it to leave insert.
        if key.code == KeyCode::Esc && mode == EditorMode::Normal {
            if let Some(b) = self.active_endpoint_buffer_mut() {
                b.url_popup = None;
            }
            return Ok(());
        }
        if let Some(b) = self.active_endpoint_buffer_mut()
            && let Some(editor) = b.url_popup.as_mut()
        {
            b.url_popup_events.on_key_event(key, editor);
        }
        Ok(())
    }

    /// Begins URL editing via `i`/`Enter`: opens the inline editor or the popup
    /// per `url_edit_mode`. A no-op when no endpoint is loaded.
    pub(in crate::tui::app) fn begin_url_edit(&mut self) {
        match self.url_edit_mode {
            UrlEditMode::Inline => self.begin_url_edit_inline(),
            UrlEditMode::Popup => self.begin_url_popup(),
        }
    }

    /// Opens the inline URL editor (seeds the LineEditor with the current URL).
    pub(in crate::tui::app) fn begin_url_edit_inline(&mut self) {
        let Some(url) = self.live_request().map(|r| r.url.clone()) else {
            self.notify("no endpoint selected");
            return;
        };
        self.set_focus(Pane::UrlBar);
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.url_editor = Some(LineEditor::new(&url));
        }
    }

    /// Opens the centered vim-popup URL editor (`e`, or `url_edit = "popup"`).
    /// Clipboard: see [`new_editor_state`] — in-memory, not the OS clipboard.
    pub(in crate::tui::app) fn begin_url_popup(&mut self) {
        let Some(url) = self.live_request().map(|r| r.url.clone()) else {
            self.notify("no endpoint selected");
            return;
        };
        self.set_focus(Pane::UrlBar);
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.url_editor = None;
            b.url_popup = Some(new_editor_state(url.as_str()));
            b.url_popup_vim.reset();
        }
    }

    /// Commits a new URL (from the inline editor or the popup): strips the query
    /// string, merges it into the Params tab, and sets the base
    /// URL. Reports the merge and marks dirty. A no-op when nothing is loaded.
    pub(in crate::tui::app) fn commit_url(&mut self, url: String) {
        let (base, pairs) = split_query(&url);
        let Some(selected) = self.selected_mut() else {
            return;
        };
        selected.endpoint.request.url = base;
        let report = merge_query_params(&mut selected.endpoint.request.params, &pairs);
        if let Some(report) = report {
            self.notify(format!("params: {report}"));
        }
    }

    /// Handles one key while editing the URL inline: Enter commits, Esc reverts,
    /// everything else goes to the LineEditor.
    pub(in crate::tui::app) fn handle_url_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                // Take the editor out first so `commit_url` can borrow `&mut self`.
                let taken = self
                    .active_endpoint_buffer_mut()
                    .and_then(|b| b.url_editor.take());
                if let Some(editor) = taken {
                    self.commit_url(editor.text());
                }
            }
            KeyCode::Esc => {
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.url_editor = None; // revert (discard the editor's text)
                }
            }
            _ => {
                if let Some(b) = self.active_endpoint_buffer_mut()
                    && let Some(editor) = b.url_editor.as_mut()
                {
                    editor.handle_key(key);
                }
            }
        }
        Ok(())
    }

    /// Cycles the loaded request's method (GET→POST→…→GET).
    pub(in crate::tui::app) fn cycle_method(&mut self) {
        if let Some(selected) = self.selected_mut() {
            let m = selected.endpoint.request.method;
            selected.endpoint.request.method = m.cycle();
        } else {
            self.message = Some(Message::new("no endpoint selected"));
        }
    }

    /// Opens the one-key method-picker menu (focuses the URL bar).
    pub(in crate::tui::app) fn open_method_menu(&mut self) {
        if self.selected().is_none() {
            self.message = Some(Message::new("no endpoint selected"));
            return;
        }
        self.focus = Pane::UrlBar;
        self.mode = Mode::MethodMenu;
    }

    /// Handles one key in the method menu: a label sets the method, Esc cancels.
    pub(in crate::tui::app) fn handle_method_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let KeyCode::Char(c) = key.code
            && let Some(method) = method_menu::method_for(c)
            && let Some(selected) = self.selected_mut()
        {
            selected.endpoint.request.method = method;
            self.mode = Mode::Normal;
        }
    }

    /// The number of rows on the active tab of the live request. On the Body
    /// tab this is `0` for a non-multipart body (no rows — the edtui surface
    /// owns it) or `1 + parts.len()` for a `Multipart` one (the type row plus
    /// one row per part).
    pub(in crate::tui::app) fn active_tab_row_count(&self) -> usize {
        let Some(b) = self.active_endpoint_buffer() else {
            return 0;
        };
        let request = b.live_request();
        match b.tabs.active {
            RequestTab::Params => request.params.len(),
            RequestTab::Headers => request.headers.len(),
            RequestTab::Auth => auth_field_count(request.auth.as_ref()),
            RequestTab::Body => match &request.body {
                Some(Body::Multipart(parts)) => 1 + parts.len(),
                _ => 0,
            },
        }
    }

    /// `a`: add a row on the Params/Headers tab (or a part on the Body tab
    /// when its kind is Multipart) and immediately edit its name.
    pub(in crate::tui::app) fn row_add(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let request = &mut b.endpoint.endpoint.request;
        let (new_row, row_count) = match b.tabs.active {
            RequestTab::Params => {
                request.params.push(Param {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                (request.params.len() - 1, request.params.len())
            }
            RequestTab::Headers => {
                request.headers.push(Header {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                (request.headers.len() - 1, request.headers.len())
            }
            RequestTab::Body => {
                let Some(Body::Multipart(parts)) = request.body.as_mut() else {
                    return; // no add-row on a non-multipart Body tab
                };
                parts.push(Part {
                    name: String::new(),
                    value: PartValue::Text(String::new()),
                });
                (parts.len(), parts.len() + 1) // +1 offset: row 0 is the type row
            }
            // Auth has no add-row.
            RequestTab::Auth => return,
        };
        b.tabs.clamp(row_count);
        // Select and edit the new row's name field.
        match b.tabs.active {
            RequestTab::Params => b.tabs.params_sel = new_row,
            RequestTab::Headers => b.tabs.headers_sel = new_row,
            RequestTab::Body => b.tabs.parts_sel = new_row,
            RequestTab::Auth => {}
        }
        b.tabs.editing = Some(FieldEdit {
            row: new_row,
            field: EditField::Name,
            editor: LineEditor::new(""),
        });
    }

    /// `d`: delete the selected row on the Params/Headers tab, or the selected
    /// part on a Multipart Body tab (row 0, the type row, cannot be deleted —
    /// mirrors the Auth kind row). No confirm.
    pub(in crate::tui::app) fn row_delete(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let sel = b.tabs.selection();
        let request = &mut b.endpoint.endpoint.request;
        let row_count = match b.tabs.active {
            RequestTab::Params if sel < request.params.len() => {
                request.params.remove(sel);
                request.params.len()
            }
            RequestTab::Headers if sel < request.headers.len() => {
                request.headers.remove(sel);
                request.headers.len()
            }
            RequestTab::Body => {
                let Some(Body::Multipart(parts)) = request.body.as_mut() else {
                    return;
                };
                if sel == 0 || sel > parts.len() {
                    return;
                }
                parts.remove(sel - 1);
                1 + parts.len()
            }
            _ => return,
        };
        b.tabs.clamp(row_count);
    }

    /// `t` (`Action::RowToggle`): toggle the selected row's `enabled` flag
    /// (Params/Headers), the ApiKey placement on the Auth tab's placement
    /// row, or a Multipart part's value kind (Text ⇄ File — the "type"
    /// column, reset to default-empty on flip, same as `set_auth_kind`'s
    /// swap-in-default-empty-fields). A no-op on the Body type row (row 0),
    /// mirroring the Auth kind row.
    pub(in crate::tui::app) fn row_toggle(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let sel = b.tabs.selection();
        let active = b.tabs.active;
        let request = &mut b.endpoint.endpoint.request;
        match active {
            RequestTab::Params => {
                if let Some(param) = request.params.get_mut(sel) {
                    param.enabled = !param.enabled;
                }
            }
            RequestTab::Headers => {
                if let Some(header) = request.headers.get_mut(sel) {
                    header.enabled = !header.enabled;
                }
            }
            RequestTab::Auth => toggle_auth_placement(request.auth.as_mut(), sel),
            RequestTab::Body => {
                if sel == 0 {
                    return;
                }
                if let Some(Body::Multipart(parts)) = request.body.as_mut()
                    && let Some(part) = parts.get_mut(sel - 1)
                {
                    part.value = match part.value {
                        PartValue::Text(_) => PartValue::File {
                            path: String::new(),
                            filename: None,
                            mime: None,
                        },
                        PartValue::File { .. } => PartValue::Text(String::new()),
                    };
                }
            }
        }
    }

    /// `Enter`/`i`: edit the selected row. On the Auth kind row, opens the auth
    /// kind picker instead; on the Body type row (row 0, Multipart only),
    /// opens the Body-type picker instead.
    pub(in crate::tui::app) fn row_edit(&mut self) {
        let Some((sel, active)) = self
            .active_endpoint_buffer()
            .map(|b| (b.tabs.selection(), b.tabs.active))
        else {
            return;
        };
        // The Auth-tab kind row (row 0) opens the kind picker.
        if active == RequestTab::Auth && sel == 0 {
            self.open_auth_kind_picker();
            return;
        }
        // The ApiKey placement row (row 3) toggles on Enter, same as Space (the
        // pinned design: "placement row toggles header/query with Space/Enter").
        if active == RequestTab::Auth && sel == 3 {
            self.row_toggle();
            return;
        }
        // The Body-tab type row (row 0, only reachable when Multipart — see
        // `body_is_multipart`'s routing gate) opens the Body-type picker, same
        // as the Auth kind row. `<leader>b` (`open_body_type_picker`) is the
        // OTHER way in, needed because a non-multipart Body tab never reaches
        // `RowEdit` at all (edtui owns `i`/`Enter` there).
        if active == RequestTab::Body && sel == 0 {
            self.open_body_type_picker();
            return;
        }
        // Auth fields have fixed labels — edit the value directly (no name→value
        // advance). Param/Header rows start on the name field.
        let start_field = if active == RequestTab::Auth {
            EditField::Value
        } else {
            EditField::Name
        };
        let Some(text) = self.current_field_text(active, sel, start_field) else {
            return;
        };
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.tabs.editing = Some(FieldEdit {
                row: sel,
                field: start_field,
                editor: LineEditor::new(&text),
            });
        }
    }

    /// The current text of a given row/field on `tab`, or `None` when out of range
    /// or not editable.
    pub(in crate::tui::app) fn current_field_text(
        &self,
        tab: RequestTab,
        row: usize,
        field: EditField,
    ) -> Option<String> {
        let request = self.live_request()?;
        let (name, value) = match tab {
            RequestTab::Params => {
                let p = request.params.get(row)?;
                (p.name.clone(), p.value.clone())
            }
            RequestTab::Headers => {
                let h = request.headers.get(row)?;
                (h.name.clone(), h.value.clone())
            }
            RequestTab::Auth => return auth_field_text(request.auth.as_ref(), row),
            RequestTab::Body => {
                // Row 0 (the type row) has no text field — it opens a picker
                // instead (see `row_edit`), never a `FieldEdit`.
                let Some(Body::Multipart(parts)) = request.body.as_ref() else {
                    return None;
                };
                let part = parts.get(row.checked_sub(1)?)?;
                (
                    part.name.clone(),
                    match &part.value {
                        PartValue::Text(text) => text.clone(),
                        PartValue::File { path, .. } => path.clone(),
                    },
                )
            }
        };
        Some(match field {
            EditField::Name => name,
            EditField::Value => value,
        })
    }

    /// Handles one key during an in-progress row field edit. Tab/Enter advance
    /// name→value (or commit the row on the value field); Esc cancels.
    pub(in crate::tui::app) fn handle_field_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                // Cancel; a never-committed freshly-added row (name+value both
                // empty) is removed, else `a`+Esc leaves a nameless ghost row
                // that serializes.
                let edit = self
                    .active_endpoint_buffer_mut()
                    .and_then(|b| b.tabs.editing.take());
                if let Some(edit) = edit {
                    self.discard_row_if_empty(edit.row);
                }
            }
            KeyCode::Tab => self.field_edit_advance(false),
            KeyCode::Enter => self.field_edit_advance(true),
            _ => {
                if let Some(b) = self.active_endpoint_buffer_mut()
                    && let Some(edit) = b.tabs.editing.as_mut()
                {
                    edit.editor.handle_key(key);
                }
            }
        }
        Ok(())
    }

    /// Removes a Params/Headers row whose stored name *and* value are both
    /// empty — the ghost a cancelled `a`(dd) would otherwise leave behind (it is
    /// nameless, enabled, and would serialize on save).
    pub(in crate::tui::app) fn discard_row_if_empty(&mut self, row: usize) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let active = b.tabs.active;
        let request = &mut b.endpoint.endpoint.request;
        let removed = match active {
            RequestTab::Params
                if request
                    .params
                    .get(row)
                    .is_some_and(|p| p.name.is_empty() && p.value.is_empty()) =>
            {
                request.params.remove(row);
                Some(request.params.len())
            }
            RequestTab::Headers
                if request
                    .headers
                    .get(row)
                    .is_some_and(|h| h.name.is_empty() && h.value.is_empty()) =>
            {
                request.headers.remove(row);
                Some(request.headers.len())
            }
            RequestTab::Body if row >= 1 => match request.body.as_mut() {
                Some(Body::Multipart(parts))
                    if parts
                        .get(row - 1)
                        .is_some_and(|p| p.name.is_empty() && part_value_is_empty(&p.value)) =>
                {
                    parts.remove(row - 1);
                    Some(1 + parts.len())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(n) = removed {
            b.tabs.clamp(n);
        }
    }

    /// Commits the current field edit into the live request. `commit_row` closes
    /// the edit after the value field; otherwise name→value advances.
    pub(in crate::tui::app) fn field_edit_advance(&mut self, commit_row: bool) {
        let Some(edit) = self
            .active_endpoint_buffer_mut()
            .and_then(|b| b.tabs.editing.take())
        else {
            return;
        };
        let active = self.active_tab();
        let text = edit.editor.text();
        self.write_field(active, edit.row, edit.field, text);
        match edit.field {
            EditField::Name => {
                // A Multipart File-part's value is a PATH, set only via the
                // file picker (never free-typed) — advancing past its name
                // opens the picker instead of a plain value LineEditor. The
                // row edit is already closed (`.take()` above); the path is
                // written on the picker's own accept, not here.
                if active == RequestTab::Body && self.body_part_is_file(edit.row) {
                    self.open_file_picker(edit.row - 1);
                    return;
                }
                // Advance to the value field, seeded with its text.
                let value = self
                    .current_field_text(active, edit.row, EditField::Value)
                    .unwrap_or_default();
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.tabs.editing = Some(FieldEdit {
                        row: edit.row,
                        field: EditField::Value,
                        editor: LineEditor::new(&value),
                    });
                }
            }
            EditField::Value => {
                if !commit_row {
                    // Tab from value wraps back to name.
                    let name = self
                        .current_field_text(active, edit.row, EditField::Name)
                        .unwrap_or_default();
                    if let Some(b) = self.active_endpoint_buffer_mut() {
                        b.tabs.editing = Some(FieldEdit {
                            row: edit.row,
                            field: EditField::Name,
                            editor: LineEditor::new(&name),
                        });
                    }
                }
                // Enter on value: editing already taken → committed.
            }
        }
    }

    /// Writes an edited field back into the live request.
    pub(in crate::tui::app) fn write_field(
        &mut self,
        tab: RequestTab,
        row: usize,
        field: EditField,
        text: String,
    ) {
        let Some(selected) = self.selected_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        match tab {
            RequestTab::Params => {
                if let Some(p) = request.params.get_mut(row) {
                    match field {
                        EditField::Name => p.name = text,
                        EditField::Value => p.value = text,
                    }
                }
            }
            RequestTab::Headers => {
                if let Some(h) = request.headers.get_mut(row) {
                    match field {
                        EditField::Name => h.name = text,
                        EditField::Value => h.value = text,
                    }
                }
            }
            RequestTab::Auth => write_auth_field(request.auth.as_mut(), row, field, text),
            RequestTab::Body => {
                if row == 0 {
                    return; // the type row has no text field
                }
                if let Some(Body::Multipart(parts)) = request.body.as_mut()
                    && let Some(part) = parts.get_mut(row - 1)
                {
                    match field {
                        EditField::Name => part.name = text,
                        // A File part's value is normally set by the file
                        // picker (see `field_edit_advance`), not here — but a
                        // Text part's value DOES land here on Enter/commit,
                        // and writing through for File too keeps this
                        // function total or a stray manual `write_field` call
                        // (e.g. a future direct binding) never silently drops
                        // a value.
                        EditField::Value => match &mut part.value {
                            PartValue::Text(t) => *t = text,
                            PartValue::File { path, .. } => *path = text,
                        },
                    }
                }
            }
        }
    }

    /// Opens the auth-kind picker (None / Basic / Bearer / ApiKey).
    pub(in crate::tui::app) fn open_auth_kind_picker(&mut self) {
        if self.selected().is_none() {
            return;
        }
        let labels = vec![
            "None".to_owned(),
            "Basic".to_owned(),
            "Bearer".to_owned(),
            "ApiKey".to_owned(),
        ];
        // The auth-kind picker is its own `Picker::Auth` variant — the accept
        // path matches the variant, not a side flag, so it can't be confused
        // with the profile/command palette.
        self.picker = Some(Picker::Auth {
            state: picker::PickerState::new(" Auth kind ", labels),
        });
        self.mode = Mode::Palette;
    }

    /// Applies an auth-kind picker choice, swapping in default-empty fields.
    pub(in crate::tui::app) fn set_auth_kind(&mut self, index: usize) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let auth = &mut b.endpoint.endpoint.request.auth;
        *auth = match index {
            0 => None,
            1 => Some(Auth::Basic {
                username: String::new(),
                password: String::new(),
            }),
            2 => Some(Auth::Bearer {
                token: String::new(),
            }),
            3 => Some(Auth::ApiKey {
                name: String::new(),
                value: String::new(),
                placement: ApiKeyPlacement::Header,
            }),
            _ => return,
        };
        b.tabs.auth_sel = 0;
    }

    /// `w`: save the live request to disk (format-preserving). Refreshes the
    /// snapshot on success; a newly-authored secret refusal surfaces on the
    /// statusline and the request stays dirty. Grandfathered/value-only findings
    /// save with a `!` warning that names them.
    pub(in crate::tui::app) fn save_request(&mut self) {
        self.sync_body_into_selected();
        let Some(selected) = self.selected() else {
            self.message = Some(Message::new("no endpoint to save"));
            return;
        };
        let path = selected.file.clone();
        let endpoint = selected.endpoint.clone();
        match persistence::save_endpoint_checked(&path, &endpoint, self.secret_policy) {
            Ok(decision) => {
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.loaded_snapshot = endpoint.clone();
                }
                self.refresh_explorer_endpoint(&path, endpoint.clone());
                let msg = if decision.warnings.is_empty() {
                    format!("Saved {}", endpoint.name)
                } else {
                    format!(
                        "Saved {} · ! secret(s) in {} — move to env/{{{{var}}}}",
                        endpoint.name,
                        decision.warning_locations().join(", ")
                    )
                };
                self.message = Some(Message::new(msg));
            }
            Err(PersistenceError::SecretsRefused { locations }) => {
                self.message = Some(Message::new(format!(
                    "not saved: new literal secret(s) ({}) — use {{{{var}}}}",
                    locations.join(", ")
                )));
            }
            Err(err) => {
                self.message = Some(Message::new(format!("save failed: {err}")));
            }
        }
    }

    /// Updates the explorer's cached copy of an endpoint after a save so the tree
    /// name and any re-selection stay consistent.
    pub(in crate::tui::app) fn refresh_explorer_endpoint(
        &mut self,
        path: &Path,
        endpoint: Endpoint,
    ) {
        self.explorer.update_endpoint(path, endpoint);
    }
}
