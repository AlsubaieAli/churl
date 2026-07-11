//! `.env` editor overlay handlers (open/key/save/close), extracted from
//! `app.rs` (M7.11). Grandchild module of `app`; `impl App` here keeps full
//! access to `App`'s private fields and methods without any visibility widening.

use super::super::*;

impl App {
    /// Opens the environments & variables editor over the current workspace.
    /// Requires an open workspace (there is nothing to edit otherwise).
    pub(in crate::tui::app) fn open_env_editor(&mut self) {
        let session = self.session_vars();
        let Some(ws) = self.workspace.as_ref() else {
            self.notify("open a workspace first");
            return;
        };
        match EnvEditorState::from_workspace(
            ws,
            self.active_profile.clone(),
            self.cli_vars.clone(),
            &session,
        ) {
            Ok(state) => {
                self.env_editor = Some(state);
                self.mode = Mode::EnvEditor;
            }
            Err(err) => self.notify(format!("couldn't open editor: {err}")),
        }
    }

    /// Routes a key to the open env editor and acts on its outcome (save / close).
    pub(in crate::tui::app) fn handle_env_editor_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(editor) = self.env_editor.as_mut() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        match editor.handle_key(key) {
            EnvKeyOutcome::Consumed => {}
            EnvKeyOutcome::Save => {
                self.env_save()?;
            }
            EnvKeyOutcome::SaveAndClose => {
                // Close only if the save actually took (a secrets refusal / IO
                // error keeps the editor open with the error visible).
                if self.env_save()? {
                    self.close_env_editor();
                }
            }
            EnvKeyOutcome::Close => self.close_env_editor(),
            EnvKeyOutcome::ClearSession => {
                // Empty the current workspace's in-memory Session store, then
                // rebuild the editor's read-only Session group so it reflects it.
                let cleared = self.clear_session_vars();
                let session = self.session_vars();
                if let Some(editor) = self.env_editor.as_mut() {
                    editor.set_session_vars(&session);
                }
                self.notify(if cleared {
                    "session captures cleared"
                } else {
                    "no session captures to clear"
                });
            }
            EnvKeyOutcome::RevealRow => {
                // Ephemeral peek (drive-test note #3): resolve the selected masked
                // row's value through the SAME resolver a standalone send uses, then
                // hand the plaintext to the editor's transient reveal state. The
                // resolved value never touches disk, a log, or any persisted field —
                // it lives only in the editor's in-memory `reveal` until re-masked.
                let raw = self
                    .env_editor
                    .as_ref()
                    .and_then(|e| e.peekable_selected_value())
                    .map(str::to_owned);
                if let Some(raw) = raw {
                    let resolved = self.build_env_resolver().substitute(&raw);
                    if let Some(editor) = self.env_editor.as_mut() {
                        editor.set_reveal(resolved);
                    }
                }
            }
            EnvKeyOutcome::CopyRevealed => {
                // Copy the revealed plaintext through the existing clipboard path
                // (the "allow copy" the owner asked for). We read it back from the
                // editor's reveal state (still live) and route it through
                // `enqueue_clipboard` — the same seam every other copy uses.
                let revealed = self
                    .env_editor
                    .as_ref()
                    .and_then(|e| e.revealed_value())
                    .map(str::to_owned);
                if let Some(value) = revealed {
                    self.enqueue_clipboard(&value, "copied revealed value");
                }
            }
            EnvKeyOutcome::CopyValue => {
                // Copy a plainly-visible (non-masked) row's value directly — the D2
                // regression fix (note #3): a visible value needs no peek to copy.
                // Same clipboard seam as every other copy; the value is taken raw,
                // exactly as the row renders it (no template resolution).
                let value = self
                    .env_editor
                    .as_ref()
                    .and_then(|e| e.selected_row_value())
                    .map(str::to_owned);
                if let Some(value) = value {
                    self.enqueue_clipboard(&value, "copied value");
                }
            }
        }
        Ok(())
    }

    /// Closes the editor and returns to normal mode.
    pub(in crate::tui::app) fn close_env_editor(&mut self) {
        self.env_editor = None;
        self.mode = Mode::Normal;
    }

    /// Runs the editor's save against the current workspace and, on success,
    /// live-refreshes the app so edits take effect without a restart. Returns
    /// whether the save succeeded (drives the save-and-close path).
    pub(in crate::tui::app) fn env_save(&mut self) -> Result<bool> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace to save into");
            return Ok(false);
        };
        let name = self
            .workspace
            .as_ref()
            .map(|ws| ws.manifest().name.clone())
            .unwrap_or_default();
        let Some(editor) = self.env_editor.as_mut() else {
            return Ok(false);
        };
        match editor.save(&root, &name) {
            EnvSaveResult::Ok { active_profile, .. } => {
                // Live-refresh: re-open the manifest and reload the explorer so the
                // send-time resolver (workspace/collection/profile vars) reflects
                // the edits immediately.
                self.active_profile = active_profile;
                self.workspace = Some(OpenWorkspace::open(&root)?);
                self.reload_explorer()?;
                self.notify("saved · vars applied");
                Ok(true)
            }
            EnvSaveResult::Refused(msg) | EnvSaveResult::Failed(msg) => {
                self.notify(msg);
                Ok(false)
            }
        }
    }
}
