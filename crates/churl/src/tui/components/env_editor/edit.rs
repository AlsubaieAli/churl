//! Key handling + edit-mode state transitions for the env editor: the
//! `handle_*_key` routers plus the var-row / profile-CRUD mutations they drive.
//! Split out of `env_editor/mod.rs` into this child module so its `impl
//! EnvEditorState` block keeps full access to the state's private fields and the
//! accessors that stay in `mod.rs`, without any visibility widening — pure
//! movement, no logic changes. `handle_key` stays `pub` (the app calls it);
//! `clamp_row` is `pub(super)` because `set_session_vars` (in `mod.rs`) calls it;
//! every other method here is called only within this module.

use super::*;

impl EnvEditorState {
    /// Handles one key event, returning what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        // A live message is cleared on the next interaction so it does not linger.
        self.message = None;

        // Ephemeral peek re-masking: ANY key other than the
        // reveal key (`p`) or the copy-while-revealed key (`y`) re-masks. This is
        // the single seam that satisfies "re-mask on cursor move to another row or
        // any mode change" — j/k, h/l, Tab, Enter, i, Esc, q, etc. all clear the
        // peek here before they act, so a revealed value can never survive a move
        // off its row or a mode switch. `p` re-peeks (refresh); `y` is handled as
        // the explicit copy below and must keep the reveal alive.
        let preserves_reveal = matches!(key.code, KeyCode::Char('p') | KeyCode::Char('y'));
        if !preserves_reveal {
            self.clear_reveal();
        }

        if self.pending_close {
            return self.handle_confirm_key(key);
        }
        if self.session_input.is_some() {
            return self.handle_session_input_key(key);
        }
        if self.naming.is_some() {
            return self.handle_naming_key(key);
        }
        if self.editing.is_some() {
            return self.handle_editing_key(key);
        }
        match self.focus {
            EnvFocus::ScopeList => self.handle_scope_key(key),
            EnvFocus::VarRows => self.handle_rows_key(key),
        }
    }

    /// Discard-confirm keys (`s` save+close, `d` discard+close, `Esc` stay).
    fn handle_confirm_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        match key.code {
            KeyCode::Char('s') => EnvKeyOutcome::SaveAndClose,
            KeyCode::Char('d') => {
                // Discard: revert the active-profile selection to what it was at
                // open (var edits are simply dropped with the state).
                self.active_profile = self.snapshot_active_profile.clone();
                EnvKeyOutcome::Close
            }
            KeyCode::Esc => {
                self.pending_close = false;
                EnvKeyOutcome::Consumed
            }
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Scope-list focus keys.
    fn handle_scope_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected_scope + 1 < self.scopes.len() {
                    self.selected_scope += 1;
                    self.selected_row = 0;
                }
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_scope = self.selected_scope.saturating_sub(1);
                self.selected_row = 0;
                EnvKeyOutcome::Consumed
            }
            // Vim `g`/`G` jump to first/last scope (aliased Home/End), matching the
            // single-`g` convention the runners use.
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected_scope = 0;
                self.selected_row = 0;
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected_scope = self.scopes.len().saturating_sub(1);
                self.selected_row = 0;
                EnvKeyOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                self.focus = EnvFocus::VarRows;
                self.clamp_row();
                EnvKeyOutcome::Consumed
            }
            // The writable Session group: `a` set · `d` delete · `c` clear-all.
            KeyCode::Char('a') if self.selected_is_session() => {
                self.start_session_var_input();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('d') if self.selected_is_session() => self.delete_selected_session_var(),
            KeyCode::Char('c') if self.selected_is_session() => EnvKeyOutcome::ClearSession,
            // Profile-only keys don't apply to Session (a flat machine-local store,
            // not a filesystem scope) — swallow with a hint.
            KeyCode::Char('n') | KeyCode::Char('r') | KeyCode::Char('x')
                if self.selected_is_session() =>
            {
                self.message = Some("Session: a set · d delete · c clear".to_owned());
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('n') => {
                self.naming = Some(ProfileNameEdit {
                    editor: LineEditor::new(""),
                    target: ProfileNameTarget::New,
                });
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('r') => {
                if matches!(self.scope().kind, EnvScopeKind::Profile) {
                    self.naming = Some(ProfileNameEdit {
                        editor: LineEditor::new(&self.scope().label),
                        target: ProfileNameTarget::Rename(self.selected_scope),
                    });
                } else {
                    self.message = Some("only profiles can be renamed here".to_owned());
                }
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('d') => {
                self.delete_profile_scope();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('x') => {
                self.set_active_profile();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('w') => EnvKeyOutcome::Save,
            KeyCode::Char('q') | KeyCode::Esc => self.request_close(),
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Var-rows focus keys.
    fn handle_rows_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let rows = self.scope().vars.len();
                if rows > 0 && self.selected_row + 1 < rows {
                    self.selected_row += 1;
                }
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_row = self.selected_row.saturating_sub(1);
                EnvKeyOutcome::Consumed
            }
            // Vim `g`/`G` jump to first/last row (aliased Home/End). Pure
            // navigation, so it works in the read-only Session group too.
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected_row = 0;
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected_row = self.scope().vars.len().saturating_sub(1);
                EnvKeyOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = EnvFocus::ScopeList;
                EnvKeyOutcome::Consumed
            }
            // The writable Session group: `a` set · `d` delete · `c` clear-all.
            // The TOML-row editors (n / Enter / i / r) don't apply — swallow with a
            // hint. `p` peek / `y` copy still fall through below.
            KeyCode::Char('a') if self.selected_is_session() => {
                self.start_session_var_input();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('d') if self.selected_is_session() => self.delete_selected_session_var(),
            KeyCode::Char('c') if self.selected_is_session() => EnvKeyOutcome::ClearSession,
            KeyCode::Char('n') | KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('r')
                if self.selected_is_session() =>
            {
                self.message = Some("Session: a set · d delete · c clear · p peek".to_owned());
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('a') | KeyCode::Char('n') => {
                self.add_row();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('d') => {
                self.delete_row();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                self.begin_edit(EnvField::Value, false);
                EnvKeyOutcome::Consumed
            }
            KeyCode::Char('r') => {
                self.begin_edit(EnvField::Name, false);
                EnvKeyOutcome::Consumed
            }
            // Ephemeral peek: reveal the selected MASKED row's
            // resolved value in place. Read-only for every scope including Session —
            // it never enters edit mode, never mutates the row. Only masked rows are
            // peekable; on a visible row it's a no-op with a hint. The app resolves
            // and calls `set_reveal` in response to `RevealRow` (the editor cannot
            // resolve — it holds no `Resolver`).
            KeyCode::Char('p') => {
                if self.peekable_selected_value().is_some() {
                    EnvKeyOutcome::RevealRow
                } else {
                    self.message = Some("nothing to peek — this value is not masked".to_owned());
                    EnvKeyOutcome::Consumed
                }
            }
            // Copy the selected value. A masked/secret row
            // keeps the never-expose-secrets stance: it must be peeked first, so `y`
            // only copies while a reveal is live (else a "press p" hint). A plainly
            // VISIBLE row has nothing to hide — `y` copies its value outright, no
            // peek needed (a non-masked value must not be uncopyable).
            KeyCode::Char('y') => {
                if self.row_is_masked(self.selected_row) {
                    if self.selected_row_is_revealed() {
                        EnvKeyOutcome::CopyRevealed
                    } else {
                        self.message = Some("nothing revealed — press p to peek first".to_owned());
                        EnvKeyOutcome::Consumed
                    }
                } else if self.selected_row_value().is_some_and(|v| !v.is_empty()) {
                    EnvKeyOutcome::CopyValue
                } else {
                    self.message = Some("nothing to copy — this value is empty".to_owned());
                    EnvKeyOutcome::Consumed
                }
            }
            KeyCode::Char('w') => EnvKeyOutcome::Save,
            KeyCode::Char('q') | KeyCode::Esc => self.request_close(),
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Keys while a var field is being edited.
    fn handle_editing_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        let Some(edit) = self.editing.as_mut() else {
            return EnvKeyOutcome::Consumed;
        };
        if edit.editor.handle_key(key) {
            // Mirror the buffer into the working row live so the render + dirty
            // state reflect in-progress typing.
            let text = edit.editor.text();
            let (row, field) = (edit.row, edit.field);
            self.write_field(row, field, text);
            return EnvKeyOutcome::Consumed;
        }
        match key.code {
            KeyCode::Enter => {
                self.commit_edit();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Tab => {
                self.toggle_edit_field();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Esc => {
                self.cancel_edit();
                EnvKeyOutcome::Consumed
            }
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Keys while the profile-name prompt is open.
    fn handle_naming_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        let Some(naming) = self.naming.as_mut() else {
            return EnvKeyOutcome::Consumed;
        };
        if naming.editor.handle_key(key) {
            return EnvKeyOutcome::Consumed;
        }
        match key.code {
            KeyCode::Enter => {
                self.commit_naming();
                EnvKeyOutcome::Consumed
            }
            KeyCode::Esc => {
                self.naming = None;
                EnvKeyOutcome::Consumed
            }
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Opens the two-step "add/set a session var" prompt (name → masked value).
    fn start_session_var_input(&mut self) {
        self.session_input = Some(SessionVarInput {
            name: String::new(),
            editor: LineEditor::new(""),
            phase: SessionPhase::Name,
        });
    }

    /// Emits a delete for the selected Session row, or a hint when none is under
    /// the cursor.
    fn delete_selected_session_var(&mut self) -> EnvKeyOutcome {
        match self.selected_session_var_name() {
            Some(name) => EnvKeyOutcome::DeleteSessionVar { name },
            None => {
                self.message = Some("no session var selected".to_owned());
                EnvKeyOutcome::Consumed
            }
        }
    }

    /// Keys while the session-var prompt is open. Name phase → Enter advances to
    /// the value phase (empty name is rejected, prompt stays open); value phase →
    /// Enter emits [`EnvKeyOutcome::SetSessionVar`] (empty value allowed). Esc
    /// cancels. The value is masked as typed by the renderer.
    fn handle_session_input_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        let Some(input) = self.session_input.as_mut() else {
            return EnvKeyOutcome::Consumed;
        };
        if input.editor.handle_key(key) {
            return EnvKeyOutcome::Consumed;
        }
        match key.code {
            KeyCode::Esc => {
                self.session_input = None;
                EnvKeyOutcome::Consumed
            }
            KeyCode::Enter => {
                let input = self
                    .session_input
                    .as_mut()
                    .expect("session_input is Some in this handler");
                match input.phase {
                    SessionPhase::Name => {
                        let name = input.editor.text().trim().to_owned();
                        if name.is_empty() {
                            // Fail loud: reject an empty name, keep the prompt open.
                            self.message = Some("session var name must not be empty".to_owned());
                            return EnvKeyOutcome::Consumed;
                        }
                        input.name = name;
                        input.phase = SessionPhase::Value;
                        input.editor = LineEditor::new("");
                        EnvKeyOutcome::Consumed
                    }
                    SessionPhase::Value => {
                        // Empty value is allowed (an explicit blank).
                        let value = input.editor.text();
                        let name = input.name.clone();
                        self.session_input = None;
                        EnvKeyOutcome::SetSessionVar { name, value }
                    }
                }
            }
            _ => EnvKeyOutcome::Consumed,
        }
    }

    /// Requests a close: immediate when clean, else arm the discard confirm.
    fn request_close(&mut self) -> EnvKeyOutcome {
        if self.is_dirty() {
            self.pending_close = true;
            EnvKeyOutcome::Consumed
        } else {
            EnvKeyOutcome::Close
        }
    }

    // --- Var-row mutations ---

    pub(super) fn clamp_row(&mut self) {
        let rows = self.scope().vars.len();
        if rows == 0 {
            self.selected_row = 0;
        } else if self.selected_row >= rows {
            self.selected_row = rows - 1;
        }
    }

    fn add_row(&mut self) {
        let scope = &mut self.scopes[self.selected_scope];
        scope.vars.push((String::new(), String::new()));
        self.selected_row = scope.vars.len() - 1;
        // A new row starts in Name edit and auto-advances into Value on commit.
        self.begin_edit(EnvField::Name, true);
    }

    fn delete_row(&mut self) {
        let scope = &mut self.scopes[self.selected_scope];
        if scope.vars.is_empty() {
            return;
        }
        scope.vars.remove(self.selected_row);
        if self.selected_row >= scope.vars.len() {
            self.selected_row = scope.vars.len().saturating_sub(1);
        }
    }

    fn begin_edit(&mut self, field: EnvField, is_new: bool) {
        if self.scope().vars.is_empty() {
            return;
        }
        let row = self.selected_row;
        let seed = match field {
            EnvField::Name => self.scope().vars[row].0.clone(),
            EnvField::Value => self.scope().vars[row].1.clone(),
        };
        self.editing = Some(EnvFieldEdit {
            row,
            field,
            editor: LineEditor::new(&seed),
            original: seed,
            is_new,
        });
    }

    fn write_field(&mut self, row: usize, field: EnvField, text: String) {
        if let Some(entry) = self.scopes[self.selected_scope].vars.get_mut(row) {
            match field {
                EnvField::Name => entry.0 = text,
                EnvField::Value => entry.1 = text,
            }
        }
    }

    fn commit_edit(&mut self) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        self.write_field(edit.row, edit.field, edit.editor.text());
        // Committing a fresh row's name auto-advances into the value edit.
        if edit.is_new && edit.field == EnvField::Name {
            self.begin_edit(EnvField::Value, false);
        }
    }

    fn toggle_edit_field(&mut self) {
        // Commit the current field into the row, then flip to the other field.
        let Some(edit) = self.editing.as_ref() else {
            return;
        };
        let (row, next) = (
            edit.row,
            match edit.field {
                EnvField::Name => EnvField::Value,
                EnvField::Value => EnvField::Name,
            },
        );
        let text = edit.editor.text();
        let field = edit.field;
        self.write_field(row, field, text);
        self.selected_row = row;
        self.begin_edit(next, false);
    }

    fn cancel_edit(&mut self) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        // Revert the live-mirrored preview back to the pre-edit value.
        self.write_field(edit.row, edit.field, edit.original.clone());
        // A fresh row left fully empty (its name was never committed) is dropped.
        if edit.is_new {
            let scope = &mut self.scopes[self.selected_scope];
            if let Some((name, value)) = scope.vars.get(edit.row)
                && name.is_empty()
                && value.is_empty()
            {
                scope.vars.remove(edit.row);
                self.clamp_row();
            }
        }
    }

    // --- Profile CRUD ---

    fn commit_naming(&mut self) {
        let Some(naming) = self.naming.take() else {
            return;
        };
        let name = naming.editor.text().trim().to_owned();
        if name.is_empty() {
            self.message = Some("profile name must not be empty".to_owned());
            return;
        }
        match naming.target {
            ProfileNameTarget::New => {
                if self.profile_exists(&name, None) {
                    self.message = Some(format!("profile {name:?} already exists"));
                    return;
                }
                self.scopes.push(EnvScope {
                    kind: EnvScopeKind::Profile,
                    label: name,
                    vars: Vec::new(),
                });
                self.selected_scope = self.scopes.len() - 1;
                self.selected_row = 0;
                self.focus = EnvFocus::VarRows;
            }
            ProfileNameTarget::Rename(idx) => {
                if self.profile_exists(&name, Some(idx)) {
                    self.message = Some(format!("profile {name:?} already exists"));
                    return;
                }
                if let Some(scope) = self.scopes.get_mut(idx) {
                    // Keep the active-profile mirror pointed at the renamed profile.
                    if self.active_profile.as_deref() == Some(scope.label.as_str()) {
                        self.active_profile = Some(name.clone());
                    }
                    scope.label = name;
                }
            }
        }
    }

    fn profile_exists(&self, name: &str, ignore: Option<usize>) -> bool {
        self.scopes.iter().enumerate().any(|(i, s)| {
            Some(i) != ignore && matches!(s.kind, EnvScopeKind::Profile) && s.label == name
        })
    }

    fn delete_profile_scope(&mut self) {
        if !matches!(self.scope().kind, EnvScopeKind::Profile) {
            self.message = Some(
                "only profiles can be deleted here (collections: use the explorer)".to_owned(),
            );
            return;
        }
        let removed = self.scopes.remove(self.selected_scope);
        if self.active_profile.as_deref() == Some(removed.label.as_str()) {
            self.active_profile = None;
        }
        if self.selected_scope >= self.scopes.len() {
            self.selected_scope = self.scopes.len().saturating_sub(1);
        }
        self.selected_row = 0;
        self.message = Some(format!("deleted profile {:?} (unsaved)", removed.label));
    }

    pub(super) fn set_active_profile(&mut self) {
        if let EnvScopeKind::Profile = self.scope().kind {
            let name = self.scope().label.clone();
            if self.active_profile.as_deref() == Some(name.as_str()) {
                self.active_profile = None;
                self.message = Some("cleared active profile".to_owned());
            } else {
                self.message = Some(format!("active profile → {name}"));
                self.active_profile = Some(name);
            }
        } else {
            self.message = Some("select a profile to activate".to_owned());
        }
    }
}
