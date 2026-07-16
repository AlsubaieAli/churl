//! CRUD / prompt / confirm handlers — endpoint & collection create/rename/
//! delete, import/export, paste-curl, copy-as-curl, and the prompt/confirm
//! key routing — extracted from `app.rs`. Grandchild module of
//! `app`; `impl App` here keeps full access to `App`'s private fields and
//! methods without any visibility widening.

use super::super::*;

impl App {
    /// `n`: prompt for a new endpoint name, created in the cursor's collection
    /// context (M7.9 cursor-aware create) — a collection under the cursor, the
    /// owning collection of the selected endpoint, or the **root** collection when
    /// nothing is selected (so `n` at the root makes a root-level endpoint).
    pub(in crate::tui::app) fn begin_new_endpoint(&mut self) {
        if self.explorer.cursor_collection_dir().is_none() {
            self.message = Some(Message::new("no workspace open"));
            return;
        }
        self.open_prompt(PromptPurpose::NewEndpoint, "");
    }

    /// `N`: prompt for a new (sub-)collection name, created under the cursor's
    /// collection context (M7.9) — a top-level collection at the root, or a
    /// sub-collection inside the collection under the cursor.
    pub(in crate::tui::app) fn begin_new_collection(&mut self) {
        if self.explorer.cursor_collection_dir().is_none() {
            self.message = Some(Message::new("no workspace open"));
            return;
        }
        self.open_prompt(PromptPurpose::NewCollection, "");
    }

    /// `r`: prompt to rename the selected endpoint or collection.
    pub(in crate::tui::app) fn begin_rename(&mut self) {
        let Some(name) = self.explorer.selected_name() else {
            self.message = Some(Message::new("nothing selected to rename"));
            return;
        };
        self.open_prompt(PromptPurpose::Rename, &name);
    }

    /// `d`: delete the selected item — y/n for an endpoint, typed name for a
    /// collection (risk-proportional friction).
    pub(in crate::tui::app) fn begin_delete(&mut self) {
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                self.mode = Mode::Confirm(ConfirmPurpose::DeleteEndpoint);
            }
            Some(RowKind::Collection) => {
                self.open_prompt(PromptPurpose::DeleteCollectionConfirm, "");
            }
            None => {
                self.message = Some(Message::new("nothing selected to delete"));
            }
        }
    }

    /// Opens a text prompt seeded with `seed`.
    pub(in crate::tui::app) fn open_prompt(&mut self, purpose: PromptPurpose, seed: &str) {
        self.prompt_editor = LineEditor::new(seed);
        self.mode = Mode::Prompt(purpose);
    }

    /// Palette: prompt for a JSON collection file path to import.
    pub(in crate::tui::app) fn begin_import_collection(&mut self) {
        if self.workspace.is_none() {
            self.notify("no workspace open");
            return;
        }
        self.open_prompt(PromptPurpose::ImportCollection, "");
    }

    /// Palette: prompt for an export destination for the selected collection.
    pub(in crate::tui::app) fn begin_export_collection(&mut self, dialect: JsonDialect) {
        let Some(name) = self.selected_collection_name() else {
            self.notify("select a collection first");
            return;
        };
        let seed = default_export_path(&name);
        self.open_prompt(PromptPurpose::ExportCollection(dialect), &seed);
    }

    /// Palette: prompt for an export destination for the whole workspace.
    pub(in crate::tui::app) fn begin_export_workspace(&mut self, dialect: JsonDialect) {
        let Some(ws) = self.workspace.as_ref() else {
            self.notify("no workspace open");
            return;
        };
        let seed = default_export_path(&ws.manifest().name);
        self.open_prompt(PromptPurpose::ExportWorkspace(dialect), &seed);
    }

    /// `<leader>n`: choose a destination collection (or the root), then the shared
    /// name prompt — a create that always asks *where* explicitly.
    pub(in crate::tui::app) fn begin_new_endpoint_pick(&mut self) {
        self.open_destination_picker(DestPurpose::CreateEndpoint, " New endpoint in ");
    }

    /// `<leader>N`: choose a destination collection (or the root) for a new
    /// (sub-)collection, then the shared name prompt.
    pub(in crate::tui::app) fn begin_new_collection_pick(&mut self) {
        self.open_destination_picker(DestPurpose::CreateCollection, " New collection in ");
    }

    /// `(label, dir)` for every collection destination: the **root first**, then
    /// every collection in load order, nested collections shown with a
    /// `parent / child` path label. Drives the create destination picker (shared,
    /// later, by move-to / copy-to).
    pub(in crate::tui::app) fn collection_destinations(&self) -> Vec<(String, PathBuf)> {
        let Some(ws) = self.workspace.as_ref() else {
            return Vec::new();
        };
        let mut out = vec![("(root)".to_owned(), ws.root().to_owned())];
        fn walk(coll: &persistence::Collection, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
            let label = if prefix.is_empty() {
                coll.name.clone()
            } else {
                format!("{prefix} / {}", coll.name)
            };
            out.push((label.clone(), coll.path.clone()));
            if let Ok(subs) = coll.sub_collections() {
                for sub in subs {
                    walk(&sub, &label, out);
                }
            }
        }
        if let Ok(cols) = ws.collections() {
            for c in &cols {
                walk(c, "", &mut out);
            }
        }
        out
    }

    /// Opens the fuzzy destination picker (root first) for `purpose`.
    pub(in crate::tui::app) fn open_destination_picker(
        &mut self,
        purpose: DestPurpose,
        title: &'static str,
    ) {
        let dests = self.collection_destinations();
        if dests.is_empty() {
            self.notify("open a workspace first");
            return;
        }
        let (labels, dirs): (Vec<String>, Vec<PathBuf>) = dests.into_iter().unzip();
        self.picker = Some(Picker::Destination {
            state: picker::PickerState::new(title, labels),
            dirs,
            purpose,
            source: None,
        });
        // Reuse the Palette routing tier (like the profile/auth pickers) — the
        // items travel with the variant, so the shared overlay handling applies.
        self.mode = Mode::Palette;
    }

    /// Imports a pasted curl into a new endpoint under `dir`, seeding
    /// method/URL/headers/body and auto-naming from the parsed request (reuses the
    /// M4 `import_curl` parser). Returns whether an endpoint was created (`false` =
    /// nothing written). On success the new endpoint is opened as its own buffer.
    pub(in crate::tui::app) fn create_endpoint_from_curl(
        &mut self,
        dir: &Path,
        curl: &str,
    ) -> Result<bool> {
        let result = match churl_core::import::import_curl(curl) {
            Ok(result) => result,
            Err(err) => {
                self.notify(format!("curl import failed: {err}"));
                return Ok(false);
            }
        };
        let path = match persistence::create_endpoint(dir, &result.endpoint.name) {
            Ok(path) => path,
            Err(err) => {
                self.crud_error(err);
                return Ok(false);
            }
        };
        // Overwrite the default endpoint with the import, keeping the
        // collection-assigned seq.
        let mut endpoint = result.endpoint;
        endpoint.seq = persistence::load_endpoint(&path)
            .map(|e| e.seq)
            .unwrap_or(0);
        if let Err(err) = persistence::save_endpoint(&path, &endpoint) {
            self.crud_error(err);
            return Ok(false);
        }
        self.reload_explorer()?;
        // Bind any placeholdered secret (Bearer `{{token}}` / `-u` `{{password}}`)
        // into a RAM-only Session var so the imported endpoint is actually
        // sendable — the workspace file still holds only the placeholder. Overwrite
        // an existing same-named session var for now (dedup/rename is deferred; see
        // docs/ROADMAP.md). Never echo the secret value.
        let captured = result.captured_secrets.clone();
        for (name, value) in captured.iter().cloned() {
            self.write_session_var(name, value);
        }
        // Surface the warning TEXT, not just a count — some warnings are
        // security-relevant (e.g. `-k` baked insecure-TLS onto the endpoint) and
        // must be loud in the TUI, matching the CLI import path.
        let mut msg = format!("imported curl → {}", endpoint.name);
        for (name, _) in &captured {
            let what = match name.as_str() {
                "token" => "Bearer token",
                "password" => "password",
                other => other,
            };
            msg.push_str(&format!(
                " · {what} captured into session var {{{{{name}}}}} (masked, this session only)"
            ));
        }
        if !result.warnings.is_empty() {
            msg.push_str(" · ");
            msg.push_str(&result.warnings.join(" · "));
        }
        self.notify(msg);
        // Open the new endpoint as its own buffer (File target — no confirm).
        self.guarded_load(PendingLoad::File(path))?;
        Ok(true)
    }

    /// The display name of the collection the selection belongs to (a collection
    /// row itself, or the collection owning the selected endpoint).
    pub(in crate::tui::app) fn selected_collection_name(&self) -> Option<String> {
        let dir = self.explorer.selected_collection_dir()?;
        dir.file_name().and_then(|n| n.to_str()).map(str::to_owned)
    }

    /// Loads the selected collection's endpoints from disk (name + endpoints).
    pub(in crate::tui::app) fn selected_collection_endpoints(
        &self,
    ) -> Option<(String, Vec<Endpoint>)> {
        let dir = self.explorer.selected_collection_dir()?;
        let name = dir.file_name()?.to_str()?.to_owned();
        let collection = persistence::Collection {
            name: name.clone(),
            path: dir,
        };
        let endpoints = collection
            .endpoints_lenient()
            .ok()?
            .endpoints
            .into_iter()
            .map(|(_, ep)| ep)
            .collect();
        Some((name, endpoints))
    }

    /// `C` / palette: copy the loaded request as a curl one-liner. `resolved`
    /// substitutes `{{var}}`s first (secrets caution — the explicit opt-in).
    pub(in crate::tui::app) fn copy_as_curl(&mut self, resolved: bool) {
        // One-shot read: fall back to the hovered endpoint when nothing is loaded,
        // so copy acts on what the cursor points at.
        let Some(selected) = self.selected().cloned().or_else(|| self.hovered_endpoint()) else {
            self.notify("no endpoint selected");
            return;
        };
        let mut endpoint = selected.endpoint.clone();
        // Fold in unsaved body edits so the copy matches what's shown.
        let body_text = self
            .active_endpoint_buffer()
            .map(|b| String::from(b.editor.lines.clone()))
            .unwrap_or_default();
        overwrite_body_text(&mut endpoint.request, body_text);
        if resolved {
            self.build_resolver(&selected)
                .substitute_request(&mut endpoint.request);
        }
        let curl = churl_core::export::export_curl(&endpoint);
        let success_msg = if resolved {
            "copied curl (vars resolved — may contain secrets)"
        } else {
            "copied curl"
        };
        self.enqueue_clipboard(&curl, success_msg);
    }

    /// Commits an import-collection prompt: read the file, map it, and write the
    /// endpoints into the workspace (shared core `write_import`).
    pub(in crate::tui::app) fn commit_import_collection(&mut self, path: String) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return Ok(());
        };
        let path = path.trim();
        if path.is_empty() {
            self.notify("no path given");
            return Ok(());
        }
        let json = match std::fs::read_to_string(path) {
            Ok(json) => json,
            Err(err) => {
                self.notify(format!("import failed: cannot read {path}: {err}"));
                return Ok(());
            }
        };
        let import = match interchange::import_json(&json) {
            Ok(import) => import,
            Err(err) => {
                self.notify(format!("import failed: {err}"));
                return Ok(());
            }
        };
        match interchange::write_import(&root, &import) {
            Ok(summary) => {
                self.reload_explorer()?;
                let mut msg = format!(
                    "imported {} endpoint(s) into {} collection(s)",
                    summary.endpoints, summary.collections
                );
                if !summary.warnings.is_empty() {
                    msg.push_str(&format!(" ({} warning(s))", summary.warnings.len()));
                }
                self.notify(msg);
            }
            Err(err) => self.notify(format!("import failed: {err}")),
        }
        Ok(())
    }

    /// Commits an export prompt for the given `scope`+`dialect` to `path` (which
    /// must resolve inside the workspace root).
    pub(in crate::tui::app) fn commit_export(&mut self, purpose: PromptPurpose, path: String) {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return;
        };
        let target = match export_target(&root, &path) {
            Ok(target) => target,
            Err(err) => {
                self.notify(format!("export failed: {err}"));
                return;
            }
        };
        // Build the export string per scope + dialect.
        let contents = match purpose {
            PromptPurpose::ExportCollection(dialect) => {
                let Some((name, endpoints)) = self.selected_collection_endpoints() else {
                    self.notify("select a collection first");
                    return;
                };
                interchange::export_collection(&name, &endpoints, dialect)
            }
            PromptPurpose::ExportWorkspace(dialect) => {
                let ws = self
                    .workspace
                    .as_ref()
                    .expect("root came from the workspace");
                interchange::export_workspace(ws, dialect)
            }
            _ => return,
        };
        let contents = match contents {
            Ok(contents) => contents,
            Err(err) => {
                self.notify(format!("export failed: {err}"));
                return;
            }
        };
        if let Some(parent) = target.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            self.notify(format!("export failed: {err}"));
            return;
        }
        match std::fs::write(&target, contents) {
            Ok(()) => {
                let shown = target
                    .strip_prefix(&root)
                    .map(crate::tui::app::rel_to_logical)
                    .unwrap_or_else(|_| target.display().to_string());
                self.notify(format!("exported to {shown}"));
            }
            Err(err) => self.notify(format!("export failed: {err}")),
        }
    }

    /// Handles one key in a text-prompt overlay.
    pub(in crate::tui::app) fn handle_prompt_key(
        &mut self,
        key: KeyEvent,
        purpose: PromptPurpose,
    ) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                // Cancelling a create abandons any picker-chosen destination, so a
                // later cursor-context `n`/`N` never reuses a stale target.
                self.pending_create_dir = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let text = self.prompt_editor.text();
                self.mode = Mode::Normal;
                self.commit_prompt(purpose, text)?;
            }
            _ => {
                self.prompt_editor.handle_key(key);
            }
        }
        Ok(())
    }

    /// Commits a text prompt: performs the CRUD op via the core seams.
    pub(in crate::tui::app) fn commit_prompt(
        &mut self,
        purpose: PromptPurpose,
        text: String,
    ) -> Result<()> {
        match purpose {
            PromptPurpose::NewEndpoint => {
                // Target = the `<leader>n` picker's chosen destination, else the
                // cursor's collection context (M7.9 fast-path — the collection under
                // the cursor, or the root when nothing is selected).
                let Some(dir) = self
                    .pending_create_dir
                    .take()
                    .or_else(|| self.explorer.cursor_collection_dir())
                else {
                    return Ok(());
                };
                // The shared name prompt auto-detects a pasted curl: a buffer
                // starting with the `curl` word imports + auto-names instead of
                // creating a blank endpoint. A parse failure is fail-loud — nothing
                // is created and the prompt re-opens with the buffer intact.
                if looks_like_curl(&text) {
                    if !self.create_endpoint_from_curl(&dir, &text)? {
                        self.pending_create_dir = Some(dir);
                        self.open_prompt(PromptPurpose::NewEndpoint, &text);
                    }
                    return Ok(());
                }
                match persistence::create_endpoint(&dir, &text) {
                    Ok(path) => {
                        self.reload_explorer()?;
                        self.message = Some(Message::new(created_message(&text, &path)));
                        // File target — no cross-endpoint confirm in the
                        // multi-buffer model.
                        self.guarded_load(PendingLoad::File(path))?;
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::NewCollection => {
                // Target = the `<leader>N` picker's chosen parent, else the cursor's
                // collection context (a sub-collection under the collection at the
                // cursor, or a top-level collection at the root).
                let Some(parent) = self
                    .pending_create_dir
                    .take()
                    .or_else(|| self.explorer.cursor_collection_dir())
                else {
                    return Ok(());
                };
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    return Ok(());
                };
                // The reserved-`sequences` bump applies only at the root.
                match persistence::create_collection(&parent, &text, &root) {
                    Ok(dir) => {
                        self.reload_explorer()?;
                        self.message = Some(Message::new(created_message(&text, &dir)));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::Rename => self.commit_rename(text)?,
            PromptPurpose::DeleteCollectionConfirm => {
                let Some((dir, name)) = self
                    .explorer
                    .selected_collection_dir()
                    .zip(self.explorer.selected_name())
                else {
                    return Ok(());
                };
                if text != name {
                    self.message = Some(Message::new("name mismatch — not deleted"));
                    return Ok(());
                }
                match persistence::delete_collection(&dir) {
                    Ok(()) => {
                        // reload_explorer's remap clears a selection whose file
                        // vanished with the collection.
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("deleted {name}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::ImportCollection => self.commit_import_collection(text)?,
            PromptPurpose::ExportCollection(_) | PromptPurpose::ExportWorkspace(_) => {
                self.commit_export(purpose, text)
            }
            PromptPurpose::NewSequence => self.commit_new_sequence(text)?,
        }
        Ok(())
    }

    /// Renames the selected endpoint or collection to `new_name`.
    pub(in crate::tui::app) fn commit_rename(&mut self, new_name: String) -> Result<()> {
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                let Some(path) = self.explorer.selected_endpoint_file() else {
                    return Ok(());
                };
                match persistence::rename_endpoint(&path, &new_name) {
                    Ok(new_path) => {
                        // Repoint sequence steps that referenced the old path (also
                        // closes the latent rename-breaks-sequences bug). The rename
                        // already landed on disk; a rewrite error is surfaced by
                        // `set_rename_message` rather than swallowed.
                        let refs = self.rewrite_step_refs(&path, &new_path);
                        let renamed_idx = self.buffer_index_for_path(&path);
                        if let Some(idx) = renamed_idx {
                            // Renaming the *loaded* endpoint: update file path +
                            // name in place so unsaved edits survive (no request-
                            // pane reload). Repoint before the reload so
                            // remap-by-path sees the live file.
                            let trimmed = new_name.trim().to_owned();
                            if let Some(b) = self.buffers[idx].as_endpoint_mut() {
                                b.endpoint.file = new_path.clone();
                                b.endpoint.endpoint.name = trimmed.clone();
                                b.loaded_snapshot.name = trimmed;
                            }
                            self.reload_explorer()?;
                            // Move the cursor onto the renamed row without
                            // touching the request pane.
                            self.explorer.select_file(&new_path)?;
                            self.set_rename_message(&new_name, &new_path, refs);
                        } else {
                            self.reload_explorer()?;
                            self.set_rename_message(&new_name, &new_path, refs);
                            // Guarded: selecting the renamed endpoint must not
                            // silently discard dirty edits on the loaded one.
                            self.guarded_load(PendingLoad::File(new_path))?;
                        }
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            Some(RowKind::Collection) => {
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    return Ok(());
                };
                // The reserved-`sequences` bump applies only for a root-level dir.
                match persistence::rename_collection(&dir, &new_name, &root) {
                    Ok(new_dir) => {
                        // Repoint sequence steps under the old dir prefix (closes the
                        // latent rename-breaks-sequences bug for collections too); a
                        // rewrite error surfaces via `set_rename_message`.
                        let refs = self.rewrite_step_refs(&dir, &new_dir);
                        // Repoint any buffer under the renamed dir into the new
                        // directory *before* the reload, or remap-by-path sees a
                        // vanished file (and the next save fails NotFound).
                        for buf in &mut self.buffers {
                            if let Some(b) = buf.as_endpoint_mut()
                                && let Ok(rest) = b.endpoint.file.strip_prefix(&dir)
                            {
                                b.endpoint.file = new_dir.join(rest);
                            }
                        }
                        self.reload_explorer()?;
                        self.set_rename_message(&new_name, &new_dir, refs);
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Handles one key in a y/n confirmation overlay.
    pub(in crate::tui::app) fn handle_confirm_key(
        &mut self,
        key: KeyEvent,
        purpose: ConfirmPurpose,
    ) -> Result<()> {
        match purpose {
            ConfirmPurpose::DeleteEndpoint => match key.code {
                KeyCode::Char('y') => {
                    self.mode = Mode::Normal;
                    if let Some(path) = self.explorer.selected_endpoint_file() {
                        match persistence::delete_endpoint(&path) {
                            Ok(()) => {
                                // reload_explorer's remap clears a selection
                                // whose file vanished.
                                self.reload_explorer()?;
                                self.message = Some(Message::new("deleted endpoint"));
                            }
                            Err(err) => self.crud_error(err),
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => self.mode = Mode::Normal,
                _ => {}
            },
            ConfirmPurpose::DeleteSequence => match key.code {
                KeyCode::Char('y') => {
                    self.mode = Mode::Normal;
                    if let Some(selected) = self.explorer.selected_sequence() {
                        let path = selected.file;
                        match persistence::delete_sequence(&path) {
                            Ok(()) => {
                                // The sequence editor/runner state lives INSIDE the
                                // `Mode::Sequence` variant. This confirm runs in
                                // `Mode::Confirm(DeleteSequence)`, so that state is
                                // definitionally absent (dropped when the surface
                                // closed) — a stale editor/runner pointing at this
                                // file is unreachable by construction, so no
                                // defensive drop is needed here.
                                // reload_explorer clamps the sub-pane cursor to the
                                // new sequence count, so selection lands on the
                                // next/previous sequence (or the empty-state
                                // affordance when none remain).
                                self.reload_explorer()?;
                                self.message = Some(Message::new("deleted sequence"));
                            }
                            Err(err) => self.crud_error(err),
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => self.mode = Mode::Normal,
                _ => {}
            },
            // The discard-changes confirm serves TWO deferred ops (mutually
            // exclusive — only one is `Some`): an endpoint/workspace switch
            // (`pending_load`) or a dirty buffer close (`pending_close`).
            ConfirmPurpose::DiscardChanges if self.pending_close.is_some() => {
                self.handle_close_confirm_key(key)?;
            }
            ConfirmPurpose::DiscardChanges => match key.code {
                KeyCode::Char('s') => {
                    self.mode = Mode::Normal;
                    // The switch destroys ALL buffers, so save EVERY dirty buffer,
                    // not just the active one (else a non-active one is lost).
                    self.save_all_dirty_buffers();
                    // Only switch once every buffer is clean: a refused save (e.g.
                    // literal secret auth) leaves that buffer dirty with the error
                    // on the statusline — switching would destroy those edits.
                    if !self.any_buffer_dirty() {
                        if let Some(target) = self.pending_load.take() {
                            self.perform_load(target)?;
                        }
                    } else {
                        self.pending_load = None; // stay put, error visible
                    }
                }
                KeyCode::Char('d') => {
                    self.mode = Mode::Normal;
                    // Discard: drop buffers so `is_dirty()` is false and the switch
                    // is not re-guarded. `perform_load` replaces/clears next.
                    self.buffers.clear();
                    self.active = 0;
                    if let Some(target) = self.pending_load.take() {
                        self.perform_load(target)?;
                    }
                }
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.pending_load = None;
                }
                _ => {}
            },
        }
        Ok(())
    }

    /// Resolves a dirty-buffer-close discard-confirm (`pending_close` is `Some`).
    /// `s` saves the target buffer then closes it iff the save took; `d` discards
    /// its edits and closes; `Esc` aborts the whole close op (already-closed clean
    /// buffers stay closed). For a close-all queue, resolving the front re-opens
    /// the confirm for the next still-dirty buffer.
    pub(in crate::tui::app) fn handle_close_confirm_key(&mut self, key: KeyEvent) -> Result<()> {
        // The buffer path currently prompting (front of the queue, or the single).
        let path = match self.pending_close.as_ref() {
            Some(PendingClose::One(p)) => p.clone(),
            Some(PendingClose::All(q)) => match q.front() {
                Some(p) => p.clone(),
                None => {
                    self.pending_close = None;
                    self.mode = Mode::Normal;
                    return Ok(());
                }
            },
            None => return Ok(()),
        };
        let Some(idx) = self.buffer_index_for_path(&path) else {
            // The target vanished (e.g. reload removed it) — treat as resolved.
            self.advance_close(true);
            return Ok(());
        };
        match key.code {
            KeyCode::Char('s') => {
                // Save operates on the active buffer, so focus the target first.
                self.active = idx;
                self.save_request();
                if !self.is_dirty() {
                    let i = self.active;
                    self.remove_buffer_at(i);
                    self.advance_close(true);
                } else {
                    // Refused save (e.g. literal secret) — abort the whole op,
                    // error visible on the statusline.
                    self.pending_close = None;
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char('d') => {
                self.remove_buffer_at(idx);
                self.advance_close(true);
            }
            KeyCode::Esc => {
                // Abort the remaining close op; already-closed clean buffers stay
                // closed. The still-open dirty buffers are untouched.
                self.pending_close = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Ok(())
    }

    /// Advances a resolved close-confirm: a single close finishes; a close-all
    /// queue drops the resolved front (`resolved` = pop it) and prompts the next
    /// dirty buffer, finishing when the queue drains.
    pub(in crate::tui::app) fn advance_close(&mut self, resolved: bool) {
        match self.pending_close.as_mut() {
            Some(PendingClose::One(_)) => {
                self.pending_close = None;
                self.mode = Mode::Normal;
            }
            Some(PendingClose::All(q)) => {
                if resolved {
                    q.pop_front();
                }
                self.prompt_next_close_in_queue();
            }
            None => self.mode = Mode::Normal,
        }
    }

    /// Surfaces a CRUD [`PersistenceError`] on the statusline (fail-loud).
    pub(in crate::tui::app) fn crud_error(&mut self, err: PersistenceError) {
        self.message = Some(Message::new(format!("error: {err}")));
    }

    /// Rewrites the sequence steps that referenced `old` (now at `new`) after a
    /// rename, returning the count rewritten — or the helper's error, so a failed
    /// rewrite is surfaced loudly (fail-loud, symmetric with the relocate path)
    /// rather than reported as a clean rename with a stranded sequence. `Ok(0)`
    /// only when there is no workspace or the paths fall outside the root.
    fn rewrite_step_refs(&mut self, old: &Path, new: &Path) -> Result<usize, PersistenceError> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return Ok(0);
        };
        let (Ok(old_rel), Ok(new_rel)) = (old.strip_prefix(&root), new.strip_prefix(&root)) else {
            return Ok(0);
        };
        persistence::retarget_sequence_steps(&root, old_rel, new_rel)
    }

    /// Sets the rename status line: the "renamed" message (with a
    /// `· N step(s) repointed` suffix) on a clean rewrite, or the rewrite error on
    /// failure (the rename itself already landed on disk, so this reports the
    /// stranded-step failure loudly instead of a false success).
    fn set_rename_message(
        &mut self,
        name: &str,
        path: &Path,
        refs: Result<usize, PersistenceError>,
    ) {
        match refs {
            Ok(repointed) => {
                let mut msg = renamed_message(name, path);
                if repointed > 0 {
                    msg.push_str(&format!(" · {repointed} step(s) repointed"));
                }
                self.message = Some(Message::new(msg));
            }
            Err(err) => self.crud_error(err),
        }
    }

    /// Whether the live prompt buffer currently reads as a pasted curl — drives
    /// the name prompt's live "Import from curl" label flip.
    pub(in crate::tui::app) fn prompt_buffer_is_curl(&self) -> bool {
        looks_like_curl(&self.prompt_editor.text())
    }

    // --- M7.12 tree CRUD: move-to / copy-to / duplicate / reorder ---

    /// Move-to (`is_move`) / copy-to: open the destination picker for the selected
    /// node (endpoint or collection), carrying it as the source. Sequences have no
    /// move-to (root-only, nowhere to relocate).
    pub(in crate::tui::app) fn begin_relocate(&mut self, is_move: bool) {
        if self.left_column_on_sequences() {
            self.notify("sequences are root-only — nowhere to move to");
            return;
        }
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                let Some(src) = self.explorer.selected_endpoint_file() else {
                    return;
                };
                let (purpose, title) = if is_move {
                    (DestPurpose::MoveEndpoint, " Move endpoint to ")
                } else {
                    (DestPurpose::CopyEndpoint, " Copy endpoint to ")
                };
                self.open_relocate_picker(purpose, src, title);
            }
            Some(RowKind::Collection) => {
                let Some(src) = self.explorer.selected_collection_dir() else {
                    return;
                };
                let (purpose, title) = if is_move {
                    (DestPurpose::MoveCollection, " Move collection to ")
                } else {
                    (DestPurpose::CopyCollection, " Copy collection to ")
                };
                self.open_relocate_picker(purpose, src, title);
            }
            None => self.notify("nothing selected to move"),
        }
    }

    /// Opens the destination picker for a move/copy, excluding the source subtree
    /// (a collection can never move into itself or one of its descendants).
    fn open_relocate_picker(&mut self, purpose: DestPurpose, source: PathBuf, title: &'static str) {
        let dests: Vec<(String, PathBuf)> = self
            .collection_destinations()
            .into_iter()
            .filter(|(_, dir)| !dir.starts_with(&source))
            .collect();
        if dests.is_empty() {
            self.notify("no destination available");
            return;
        }
        let (labels, dirs): (Vec<String>, Vec<PathBuf>) = dests.into_iter().unzip();
        self.picker = Some(Picker::Destination {
            state: picker::PickerState::new(title, labels),
            dirs,
            purpose,
            source: Some(source),
        });
        self.mode = Mode::Palette;
    }

    /// Moves/copies the endpoint `source` into `dest`. A move rewrites referencing
    /// sequence steps and repoints an open buffer for the moved file; a copy never
    /// rewrites (the original stays referenced).
    pub(in crate::tui::app) fn relocate_endpoint(
        &mut self,
        source: PathBuf,
        dest: PathBuf,
        is_move: bool,
    ) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return Ok(());
        };
        let result = if is_move {
            persistence::move_endpoint(&source, &dest)
        } else {
            persistence::copy_endpoint(&source, &dest)
        };
        match result {
            Ok(new_path) => {
                if is_move && new_path == source {
                    self.notify("already in that collection");
                    return Ok(());
                }
                let mut msg = if is_move {
                    "moved endpoint".to_owned()
                } else {
                    "copied endpoint".to_owned()
                };
                if is_move {
                    // Repoint an open buffer for the moved file (unsaved edits
                    // survive), then rewrite referencing sequence steps.
                    for buf in &mut self.buffers {
                        if let Some(b) = buf.as_endpoint_mut()
                            && b.endpoint.file == source
                        {
                            b.endpoint.file = new_path.clone();
                        }
                    }
                    if let (Ok(old_rel), Ok(new_rel)) =
                        (source.strip_prefix(&root), new_path.strip_prefix(&root))
                    {
                        match persistence::retarget_sequence_steps(&root, old_rel, new_rel) {
                            Ok(n) if n > 0 => msg.push_str(&format!(" · {n} step(s) repointed")),
                            Ok(_) => {}
                            Err(err) => {
                                self.crud_error(err);
                                return Ok(());
                            }
                        }
                    }
                }
                self.reload_explorer()?;
                self.notify(msg);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
    }

    /// Moves/copies the collection subtree `source` under `dest`. A move rewrites
    /// steps whose path lies under the old prefix and repoints open buffers; a copy
    /// never rewrites.
    pub(in crate::tui::app) fn relocate_collection(
        &mut self,
        source: PathBuf,
        dest: PathBuf,
        is_move: bool,
    ) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return Ok(());
        };
        let result = if is_move {
            persistence::move_collection(&source, &dest, &root)
        } else {
            persistence::copy_collection(&source, &dest, &root)
        };
        match result {
            Ok(new_dir) => {
                if is_move && new_dir == source {
                    self.notify("already there");
                    return Ok(());
                }
                let mut msg = if is_move {
                    "moved collection".to_owned()
                } else {
                    "copied collection".to_owned()
                };
                if is_move {
                    for buf in &mut self.buffers {
                        if let Some(b) = buf.as_endpoint_mut()
                            && let Ok(rest) = b.endpoint.file.strip_prefix(&source)
                        {
                            b.endpoint.file = new_dir.join(rest);
                        }
                    }
                    if let (Ok(old_rel), Ok(new_rel)) =
                        (source.strip_prefix(&root), new_dir.strip_prefix(&root))
                    {
                        match persistence::retarget_sequence_steps(&root, old_rel, new_rel) {
                            Ok(n) if n > 0 => msg.push_str(&format!(" · {n} step(s) repointed")),
                            Ok(_) => {}
                            Err(err) => {
                                self.crud_error(err);
                                return Ok(());
                            }
                        }
                    }
                }
                self.reload_explorer()?;
                self.notify(msg);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
    }

    /// Duplicates the selected node in place (`-N`): endpoint, collection subtree,
    /// or (on the sequences sub-pane) the hovered sequence.
    pub(in crate::tui::app) fn duplicate_selected(&mut self) -> Result<()> {
        if self.left_column_on_sequences() {
            let Some(sel) = self.explorer.selected_sequence() else {
                self.notify("no sequence selected");
                return Ok(());
            };
            match persistence::duplicate_sequence(&sel.file) {
                Ok(_) => {
                    self.reload_explorer()?;
                    self.notify("duplicated sequence");
                }
                Err(err) => self.crud_error(err),
            }
            return Ok(());
        }
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                let Some(file) = self.explorer.selected_endpoint_file() else {
                    return Ok(());
                };
                match persistence::duplicate_endpoint(&file) {
                    Ok(_) => {
                        self.reload_explorer()?;
                        self.notify("duplicated endpoint");
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            Some(RowKind::Collection) => {
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    self.notify("no workspace open");
                    return Ok(());
                };
                match persistence::duplicate_collection(&dir, &root) {
                    Ok(_) => {
                        self.reload_explorer()?;
                        self.notify("duplicated collection");
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            None => self.notify("nothing selected to duplicate"),
        }
        Ok(())
    }

    /// Reorders the selected node one slot among its siblings (endpoint,
    /// collection, or hovered sequence). Group-internal + parent-preserving; edge
    /// hits ("already first/last") surface a status line.
    pub(in crate::tui::app) fn reorder_selected(
        &mut self,
        direction: persistence::ReorderDir,
    ) -> Result<()> {
        if self.left_column_on_sequences() {
            let Some(sel) = self.explorer.selected_sequence() else {
                self.notify("no sequence selected");
                return Ok(());
            };
            let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                self.notify("no workspace open");
                return Ok(());
            };
            let file = sel.file.clone();
            let seq_dir = root.join("sequences");
            match persistence::reorder_sequence(&seq_dir, &file, direction) {
                Ok(outcome) => {
                    self.reload_explorer()?;
                    self.explorer.select_sequence_file(&file);
                    self.notify_reorder(outcome);
                }
                Err(err) => self.crud_error(err),
            }
            return Ok(());
        }
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                let Some(file) = self.explorer.selected_endpoint_file() else {
                    return Ok(());
                };
                // The endpoint's own collection dir is its parent — no workspace
                // lookup needed for the sibling group.
                let Some(coll) = file.parent().map(Path::to_owned) else {
                    return Ok(());
                };
                match persistence::reorder_endpoint(&coll, &file, direction) {
                    Ok(outcome) => {
                        self.reload_explorer()?;
                        // Keep the cursor on the moved endpoint (no request reload).
                        let _ = self.explorer.select_file(&file);
                        self.notify_reorder(outcome);
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            Some(RowKind::Collection) => {
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    self.notify("no workspace open");
                    return Ok(());
                };
                match persistence::reorder_collection(&dir, &root, direction) {
                    Ok(outcome) => {
                        self.reload_explorer()?;
                        self.notify_reorder(outcome);
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            None => self.notify("nothing selected to reorder"),
        }
        Ok(())
    }

    /// Surfaces the reorder edge feedback (a successful swap is silent — the tree
    /// re-renders in the new order).
    fn notify_reorder(&mut self, outcome: persistence::ReorderOutcome) {
        match outcome {
            persistence::ReorderOutcome::Moved => {}
            persistence::ReorderOutcome::AlreadyFirst => self.notify("already first"),
            persistence::ReorderOutcome::AlreadyLast => self.notify("already last"),
        }
    }
}

/// Whether a name-prompt buffer is a pasted curl command: it starts with the
/// `curl` word (a word boundary, so `curls` / `curling` do not match). Drives the
/// shared name prompt's blank-vs-import branch and its live label flip.
pub(in crate::tui::app) fn looks_like_curl(text: &str) -> bool {
    text.trim_start()
        .strip_prefix("curl")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}
