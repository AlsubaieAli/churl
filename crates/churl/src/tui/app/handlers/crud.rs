//! CRUD / prompt / confirm handlers — endpoint & collection create/rename/
//! delete, import/export, paste-curl, copy-as-curl, and the prompt/confirm
//! key routing — extracted from `app.rs` (M7.11, PR 4). Grandchild module of
//! `app`; `impl App` here keeps full access to `App`'s private fields and
//! methods without any visibility widening.

use super::super::*;

impl App {
    /// `n`: prompt for a new endpoint name (under the selected collection).
    pub(in crate::tui::app) fn begin_new_endpoint(&mut self) {
        if self.explorer.selected_collection_dir().is_none() {
            self.message = Some(Message::new("select a collection first"));
            return;
        }
        self.open_prompt(PromptPurpose::NewEndpoint, "");
    }

    /// `N`: prompt for a new collection name.
    pub(in crate::tui::app) fn begin_new_collection(&mut self) {
        if self.workspace.is_none() {
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

    /// Palette: prompt for a curl command to import as a new endpoint.
    pub(in crate::tui::app) fn begin_paste_curl(&mut self) {
        if self.explorer.selected_collection_dir().is_none() {
            self.notify("select a collection first");
            return;
        }
        self.open_prompt(PromptPurpose::PasteCurl, "");
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
        // One-shot read: fall back to the hovered endpoint when nothing is loaded
        // (M7.10 stage B), so copy acts on what the cursor points at.
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
        let import = match interchange::import_postman_v21(&json) {
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
                let shown = target.strip_prefix(&root).unwrap_or(&target);
                self.notify(format!("exported to {}", shown.display()));
            }
            Err(err) => self.notify(format!("export failed: {err}")),
        }
    }

    /// Commits a paste-curl prompt: import the curl command and create an
    /// endpoint in the selected collection.
    pub(in crate::tui::app) fn commit_paste_curl(&mut self, curl: String) -> Result<()> {
        let Some(dir) = self.explorer.selected_collection_dir() else {
            self.notify("select a collection first");
            return Ok(());
        };
        let result = match churl_core::import::import_curl(&curl) {
            Ok(result) => result,
            Err(err) => {
                self.notify(format!("curl import failed: {err}"));
                return Ok(());
            }
        };
        let path = match persistence::create_endpoint(&dir, &result.endpoint.name) {
            Ok(path) => path,
            Err(err) => {
                self.crud_error(err);
                return Ok(());
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
            return Ok(());
        }
        self.reload_explorer()?;
        let mut msg = format!("pasted curl → {}", endpoint.name);
        if !result.warnings.is_empty() {
            msg.push_str(&format!(" ({} warning(s))", result.warnings.len()));
        }
        self.notify(msg);
        // Open the new endpoint as its own buffer (File target — no confirm).
        self.guarded_load(PendingLoad::File(path))?;
        Ok(())
    }

    /// Handles one key in a text-prompt overlay.
    pub(in crate::tui::app) fn handle_prompt_key(
        &mut self,
        key: KeyEvent,
        purpose: PromptPurpose,
    ) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
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
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
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
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    return Ok(());
                };
                match persistence::create_collection(&root, &text) {
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
            PromptPurpose::PasteCurl => self.commit_paste_curl(text)?,
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
                            self.message =
                                Some(Message::new(renamed_message(&new_name, &new_path)));
                        } else {
                            self.reload_explorer()?;
                            self.message =
                                Some(Message::new(renamed_message(&new_name, &new_path)));
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
                match persistence::rename_collection(&dir, &new_name) {
                    Ok(new_dir) => {
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
                        self.message = Some(Message::new(renamed_message(&new_name, &new_dir)));
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
                                // A sequence editor/runner is a modal overlay
                                // (`Mode::Sequence`) and so cannot be open while this
                                // confirm runs in `Mode::Normal`; but if a stale
                                // editor/runner state still points at the just-deleted
                                // file, drop it so a later save can't resurrect the
                                // file and the runner can't act on a vanished target.
                                if self
                                    .sequence_editor
                                    .as_ref()
                                    .is_some_and(|e| e.path() == path)
                                {
                                    self.sequence_editor = None;
                                }
                                if self
                                    .sequence_runner
                                    .as_ref()
                                    .is_some_and(|r| r.path == path)
                                {
                                    self.sequence_runner = None;
                                }
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
}
