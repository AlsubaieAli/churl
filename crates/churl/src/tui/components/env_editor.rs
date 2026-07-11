//! The environments & variables editor (M7.3): a split-view modal for editing the
//! three template-var scopes — workspace `[vars]`, per-collection `folder.toml
//! [vars]`, and named profiles — with profile CRUD, explicit save, a
//! dirty/discard guard, secret masking + refusal, and a live **precedence
//! display** ("which value wins").
//!
//! All UI state lives here (the `churl` crate); `churl-core` stays TUI-free. Reuses
//! the core seams verbatim: [`Workspace`]/[`Profile`]/[`CollectionMeta`] as the
//! data model, the format-preserving `save_workspace_manifest`/
//! `save_collection_meta` writers (which prune deleted keys/profiles), the
//! `config` secret gates, and the [`Resolver`]-order precedence (cli > profile >
//! collection > workspace > env).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use churl_core::config::{
    collection_secret_violations, is_template_placeholder, looks_like_secret_name,
    secret_violations,
};
use churl_core::model::{CollectionMeta, Profile, Workspace};
use churl_core::persistence::{
    OpenWorkspace, PersistenceError, load_collection_meta, save_collection_meta,
    save_workspace_manifest,
};

use super::line_editor::LineEditor;
use super::prompt;
use crate::tui::theme::Theme;

/// The kind of a scope shown in the editor's left column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvScopeKind {
    /// The workspace-level `[vars]` in `churl.toml`.
    Workspace,
    /// A collection's `folder.toml [vars]`, identified by its directory.
    Collection {
        /// The collection directory (holds/receives `folder.toml`).
        dir: PathBuf,
    },
    /// A named profile's `[profiles.vars]`.
    Profile,
    /// The in-memory Session captures for the current workspace (note #6). A
    /// **read-only** display group: values are populated by sequence runs, masked,
    /// never edited here, and never written to disk. A clear action empties it.
    Session,
}

/// One editable scope: an ordered list of `(name, value)` var rows plus a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvScope {
    /// Which var scope this row-set targets on save.
    pub kind: EnvScopeKind,
    /// Display label: `"Workspace"`, the collection name, or the profile name.
    pub label: String,
    /// Ordered editable rows (a `BTreeMap` on disk; a `Vec` here for stable
    /// editing UX — insertion order, in-place rename).
    pub vars: Vec<(String, String)>,
}

/// Which pane of the editor currently has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvFocus {
    /// The left scope list.
    ScopeList,
    /// The right var-row list.
    VarRows,
}

/// Which field of a var row is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvField {
    /// The variable name (left column).
    Name,
    /// The variable value (right column).
    Value,
}

/// An in-progress field edit inside the var-row pane.
#[derive(Debug, Clone)]
pub struct EnvFieldEdit {
    /// Row being edited (index into the selected scope's `vars`).
    pub row: usize,
    /// Which field is under the cursor.
    pub field: EnvField,
    /// The shared single-line editor.
    pub editor: LineEditor,
    /// The field's value before this edit began, so `Esc` can revert the
    /// live-mirrored preview (see [`EnvEditorState::cancel_edit`]).
    pub original: String,
    /// True for a freshly-added row, so committing the name auto-advances into
    /// the value.
    pub is_new: bool,
}

/// Target of a profile-name prompt (`n` new / `r` rename).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileNameTarget {
    /// Creating a brand-new profile.
    New,
    /// Renaming the profile scope at this index.
    Rename(usize),
}

/// An open profile-name prompt (a small centered input over the modal).
#[derive(Debug, Clone)]
pub struct ProfileNameEdit {
    /// The line editor holding the typed name.
    pub editor: LineEditor,
    /// What the commit will do.
    pub target: ProfileNameTarget,
}

/// What the app should do after the editor handled a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKeyOutcome {
    /// Fully handled inside the editor; nothing for the app to do.
    Consumed,
    /// Run a save (`w`); keep the editor open.
    Save,
    /// Save then close only if the save succeeds (the discard-confirm `s`).
    SaveAndClose,
    /// Close the editor now (discard, or a clean close).
    Close,
    /// Clear the current workspace's in-memory Session captures (note #6). The app
    /// empties its store, then the editor rebuilds the Session group's rows.
    ClearSession,
    /// Ephemeral peek (drive-test note #3): the user pressed the reveal key on a
    /// masked row. The editor cannot resolve values itself (it is UI-only), so it
    /// asks the app to resolve the selected row's value through the normal
    /// [`Resolver`] and hand it back via [`EnvEditorState::set_reveal`]. Nothing is
    /// revealed until the app answers.
    RevealRow,
    /// Copy the currently-revealed value to the clipboard (`y` while a peek is
    /// active). The app reads [`EnvEditorState::revealed_value`] and routes it
    /// through the existing clipboard path. A no-op for the app when nothing is
    /// revealed (the editor only emits this while a reveal is live).
    CopyRevealed,
}

/// Default lifetime of an ephemeral secret peek, in seconds — deliberately short
/// so a revealed secret does not linger on screen. Checked on the app's existing
/// 250 ms tick (mirrors [`super::message::MESSAGE_EXPIRE_SECS`]).
pub const REVEAL_EXPIRE_SECS: u64 = 6;

/// An active ephemeral peek (drive-test note #3): exactly one masked row's
/// **resolved** value revealed in place. This is the ONLY place the plaintext of a
/// masked value lives in view state, and it is cleared on any row/scope move, mode
/// change, or timeout — never persisted, never logged. Pinned to a `(scope, row)`
/// coordinate so a stale reveal can never paint over a different row.
#[derive(Debug, Clone)]
pub struct Reveal {
    /// The scope index the reveal is pinned to.
    scope: usize,
    /// The row index the reveal is pinned to.
    row: usize,
    /// The resolved plaintext, held transiently for display + the explicit copy.
    value: String,
    /// When the reveal began, for the short auto-remask timeout.
    revealed_at: Instant,
}

/// Full state of the open environments & variables editor.
#[derive(Debug, Clone)]
pub struct EnvEditorState {
    /// All editable scopes: workspace first, then collections, then profiles.
    pub scopes: Vec<EnvScope>,
    /// Pristine clone at open, for dirty derivation.
    snapshot: Vec<EnvScope>,
    /// Selected scope (index into `scopes`).
    pub selected_scope: usize,
    /// Which pane has focus.
    pub focus: EnvFocus,
    /// Selected var row within the selected scope.
    pub selected_row: usize,
    /// In-progress var-field edit, if any.
    pub editing: Option<EnvFieldEdit>,
    /// In-progress profile-name prompt, if any.
    pub naming: Option<ProfileNameEdit>,
    /// Inline status/error message shown in the editor footer.
    pub message: Option<String>,
    /// The active ephemeral secret peek (drive-test note #3), if any. At most one
    /// row is ever revealed. Cleared on row/scope move, mode change, and timeout;
    /// its plaintext lives only here, transiently, and is never written or logged.
    reveal: Option<Reveal>,
    /// True → render the discard confirm instead of accepting close.
    pub pending_close: bool,
    /// Mirror of the app's active profile, for the precedence display; may be
    /// changed with `x` and applied to the app on save.
    pub active_profile: Option<String>,
    /// The active profile as of the last open/save, so activating a different
    /// profile with `x` counts toward dirtiness (and discard can revert it).
    snapshot_active_profile: Option<String>,
    /// CLI `--var` overrides (highest-precedence scope), for precedence display.
    cli_vars: BTreeMap<String, String>,
}

impl EnvEditorState {
    /// Builds the editor state from an opened workspace: the workspace scope
    /// first, then one scope per collection (its `folder.toml [vars]`, empty when
    /// absent), then one scope per profile.
    pub fn from_workspace(
        ws: &OpenWorkspace,
        active_profile: Option<String>,
        cli_vars: BTreeMap<String, String>,
        session_vars: &BTreeMap<String, String>,
    ) -> Result<Self, PersistenceError> {
        let manifest = ws.manifest();
        let mut scopes = Vec::new();

        scopes.push(EnvScope {
            kind: EnvScopeKind::Workspace,
            label: "Workspace".to_owned(),
            vars: map_to_rows(&manifest.vars),
        });

        for collection in ws.collections()? {
            let meta = load_collection_meta(&collection.path)?;
            scopes.push(EnvScope {
                kind: EnvScopeKind::Collection {
                    dir: collection.path.clone(),
                },
                label: collection.name.clone(),
                vars: map_to_rows(&meta.vars),
            });
        }

        for profile in &manifest.profiles {
            scopes.push(EnvScope {
                kind: EnvScopeKind::Profile,
                label: profile.name.clone(),
                vars: map_to_rows(&profile.vars),
            });
        }

        // The read-only Session group (note #6): the current workspace's in-memory
        // captures. Always present so the user can see (and clear) captured
        // secrets even when empty. Never editable, never saved.
        scopes.push(EnvScope {
            kind: EnvScopeKind::Session,
            label: "Session".to_owned(),
            vars: map_to_rows(session_vars),
        });

        Ok(Self {
            snapshot: scopes.clone(),
            scopes,
            selected_scope: 0,
            focus: EnvFocus::ScopeList,
            selected_row: 0,
            editing: None,
            naming: None,
            message: None,
            reveal: None,
            pending_close: false,
            snapshot_active_profile: active_profile.clone(),
            active_profile,
            cli_vars,
        })
    }

    /// Replaces the read-only Session group's rows from a fresh capture map (after
    /// the app clears its in-memory store). Keeps the snapshot in lockstep so this
    /// never registers as a dirtying edit — the Session group is never saved.
    pub fn set_session_vars(&mut self, session_vars: &BTreeMap<String, String>) {
        let rows = map_to_rows(session_vars);
        for scopes in [&mut self.scopes, &mut self.snapshot] {
            if let Some(scope) = scopes
                .iter_mut()
                .find(|s| matches!(s.kind, EnvScopeKind::Session))
            {
                scope.vars = rows.clone();
            }
        }
        self.clamp_row();
    }

    /// Whether the working state differs from the pristine snapshot: any var/scope
    /// edit, or a change to the active profile (`x`).
    pub fn is_dirty(&self) -> bool {
        self.scopes != self.snapshot || self.active_profile != self.snapshot_active_profile
    }

    fn scope(&self) -> &EnvScope {
        &self.scopes[self.selected_scope]
    }

    /// Whether the selected scope is the read-only Session group (note #6).
    fn selected_is_session(&self) -> bool {
        matches!(self.scope().kind, EnvScopeKind::Session)
    }

    /// Whether the row at `(selected_scope, row)` renders masked — i.e. the peek
    /// key has something to reveal. This is the SAME predicate `render_var_line`
    /// uses to decide masking, kept in one place so reveal and mask can never
    /// disagree: a Session capture (always masked) or a secret-named literal that
    /// is not a `{{placeholder}}`. Empty values are never masked (nothing to hide).
    fn row_is_masked(&self, row: usize) -> bool {
        let Some((name, value)) = self.scope().vars.get(row) else {
            return false;
        };
        !value.is_empty()
            && (matches!(self.scope().kind, EnvScopeKind::Session)
                || (looks_like_secret_name(name) && !is_template_placeholder(value)))
    }

    /// The raw (pre-resolution) value of the selected row, used by the app to
    /// resolve the reveal. `None` when there is no such row or it is not masked
    /// (only masked rows are peekable — an already-visible value needs no peek).
    pub fn peekable_selected_value(&self) -> Option<&str> {
        if !self.row_is_masked(self.selected_row) {
            return None;
        }
        self.scope()
            .vars
            .get(self.selected_row)
            .map(|(_, v)| v.as_str())
    }

    /// Records a resolved plaintext as the active peek, pinned to the currently
    /// selected `(scope, row)`. Called by the app in response to
    /// [`EnvKeyOutcome::RevealRow`]. Replaces any prior reveal (only one at a time).
    pub fn set_reveal(&mut self, value: String) {
        self.reveal = Some(Reveal {
            scope: self.selected_scope,
            row: self.selected_row,
            value,
            revealed_at: Instant::now(),
        });
    }

    /// The currently-revealed plaintext, if a peek is live AND still pinned to the
    /// selected row. Used by the app for the explicit `y` copy. `None` re-masks the
    /// copy path — you can only copy what is actually on screen.
    pub fn revealed_value(&self) -> Option<&str> {
        self.reveal
            .as_ref()
            .filter(|r| r.scope == self.selected_scope && r.row == self.selected_row)
            .map(|r| r.value.as_str())
    }

    /// Clears the active peek immediately (re-masks). Idempotent. Dropping the
    /// `Reveal` drops its plaintext `String` — nothing lingers in view state.
    fn clear_reveal(&mut self) {
        self.reveal = None;
    }

    /// Whether a peek is currently live and pinned to the selected row (drives the
    /// on-screen "revealed" affordance + the reveal-aware value rendering).
    fn selected_row_is_revealed(&self) -> bool {
        self.reveal
            .as_ref()
            .is_some_and(|r| r.scope == self.selected_scope && r.row == self.selected_row)
    }

    /// Expires the peek if it has outlived [`REVEAL_EXPIRE_SECS`]. Called by the
    /// app on its 250 ms tick (the same cadence that expires transient messages).
    /// Returns whether it cleared a reveal (so the caller can request a redraw).
    pub fn expire_reveal(&mut self) -> bool {
        if self
            .reveal
            .as_ref()
            .is_some_and(|r| r.revealed_at.elapsed().as_secs() >= REVEAL_EXPIRE_SECS)
        {
            self.reveal = None;
            true
        } else {
            false
        }
    }

    /// Handles one key event, returning what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> EnvKeyOutcome {
        // A live message is cleared on the next interaction so it does not linger.
        self.message = None;

        // Ephemeral peek re-masking (drive-test note #3): ANY key other than the
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
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                self.focus = EnvFocus::VarRows;
                self.clamp_row();
                EnvKeyOutcome::Consumed
            }
            // Clear the read-only Session group's captures. Only meaningful while
            // the Session scope is selected.
            KeyCode::Char('c') if self.selected_is_session() => EnvKeyOutcome::ClearSession,
            // The Session group is read-only: swallow new/rename/delete/activate
            // with a message (navigation, save, and close still fall through).
            KeyCode::Char('n') | KeyCode::Char('r') | KeyCode::Char('d') | KeyCode::Char('x')
                if self.selected_is_session() =>
            {
                self.message =
                    Some("Session vars are read-only (run-populated) — c to clear".to_owned());
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
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = EnvFocus::ScopeList;
                EnvKeyOutcome::Consumed
            }
            // The Session group's rows are read-only (run-populated, masked). Block
            // add/delete/edit with a message; navigation + close still work.
            KeyCode::Char('a')
            | KeyCode::Char('n')
            | KeyCode::Char('d')
            | KeyCode::Enter
            | KeyCode::Char('i')
            | KeyCode::Char('r')
                if self.selected_is_session() =>
            {
                self.message = Some(
                    "Session vars are read-only (run-populated) — h to scopes, c to clear"
                        .to_owned(),
                );
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
            // Ephemeral peek (drive-test note #3): reveal the selected MASKED row's
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
            // Copy the revealed value (the "allow copy" the owner asked for). Only
            // while a peek is live on the selected row; otherwise a no-op hint so
            // `y` never silently copies a masked/absent value.
            KeyCode::Char('y') => {
                if self.selected_row_is_revealed() {
                    EnvKeyOutcome::CopyRevealed
                } else {
                    self.message = Some("nothing revealed — press p to peek first".to_owned());
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

    fn clamp_row(&mut self) {
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

    fn set_active_profile(&mut self) {
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

    // --- Save ---

    /// Reconciles the working scopes into a [`Workspace`] + collection metas,
    /// validates secrets, and writes the changed targets (format-preserving,
    /// deletion-pruning). Refuses (writes nothing) on any secret violation. On
    /// success, refreshes the dirty snapshot and returns the new manifest for the
    /// app to apply live.
    pub fn save(&mut self, root: &Path, workspace_name: &str) -> EnvSaveResult {
        // Refuse duplicate var names before anything else: on save the rows
        // collapse to a `BTreeMap` (last wins), which would silently drop a
        // visible row. Name the scope + var and write nothing.
        if let Some(dup) = self.duplicate_name_violation() {
            let msg = format!("{dup} — rename or remove the duplicate before saving");
            self.message = Some(msg.clone());
            return EnvSaveResult::Refused(msg);
        }

        let workspace = self.build_workspace(workspace_name);
        let collections = self.build_collection_metas();

        // Validate ALL secrets before writing anything.
        let mut violations = secret_violations(&workspace);
        for (dir, meta) in &collections {
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<collection>");
            for v in collection_secret_violations(meta) {
                violations.push(format!("{name}.{v}"));
            }
        }
        if !violations.is_empty() {
            // D1 (interim): name the offending var(s) and signal they're
            // pre-existing so the refusal doesn't read as a silent dead-end. The
            // actual grandfather+warn / save-anyway behavior is R3.
            let msg = format!(
                "pre-existing secret-named var(s) with literal values not saved: {} — move them to env (grandfathering coming soon)",
                violations.join(", ")
            );
            self.message = Some(msg.clone());
            return EnvSaveResult::Refused(msg);
        }

        // Write the manifest only when workspace/profile scopes changed.
        let manifest_changed = self.manifest_scopes_changed();
        if manifest_changed && let Err(err) = save_workspace_manifest(root, &workspace) {
            let msg = format!("save failed (churl.toml): {err}");
            self.message = Some(msg.clone());
            return EnvSaveResult::Failed(msg);
        }

        // Write each changed collection meta; report the first IO failure loudly
        // (earlier writes already landed — do not clear their dirty state).
        let mut written = Vec::new();
        for (dir, meta) in &collections {
            if !self.collection_scope_changed(dir) {
                continue;
            }
            if let Err(err) = save_collection_meta(dir, meta) {
                let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let msg = format!(
                    "save partially failed: wrote {}, then {name}/folder.toml: {err}",
                    describe_written(manifest_changed, &written)
                );
                self.message = Some(msg.clone());
                return EnvSaveResult::Failed(msg);
            }
            written.push(dir.clone());
        }

        // Success: the working state is now the pristine state.
        self.snapshot = self.scopes.clone();
        self.snapshot_active_profile = self.active_profile.clone();
        EnvSaveResult::Ok {
            workspace,
            active_profile: self.active_profile.clone(),
        }
    }

    /// The first duplicate (trimmed, non-empty) var name within any scope, as a
    /// `"duplicate var name 'x' in <scope>"` message, or `None` when clean.
    fn duplicate_name_violation(&self) -> Option<String> {
        for scope in &self.scopes {
            let mut seen = std::collections::HashSet::new();
            for (name, _) in &scope.vars {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !seen.insert(trimmed.to_owned()) {
                    return Some(format!("duplicate var name {trimmed:?} in {}", scope.label));
                }
            }
        }
        None
    }

    /// Rebuilds a [`Workspace`] from the workspace + profile scopes.
    fn build_workspace(&self, name: &str) -> Workspace {
        let mut vars = BTreeMap::new();
        let mut profiles = Vec::new();
        for scope in &self.scopes {
            match &scope.kind {
                EnvScopeKind::Workspace => vars = rows_to_map(&scope.vars),
                EnvScopeKind::Profile => profiles.push(Profile {
                    name: scope.label.clone(),
                    vars: rows_to_map(&scope.vars),
                }),
                // Collection metas write separately; the Session group is
                // in-memory only and never reaches disk.
                EnvScopeKind::Collection { .. } | EnvScopeKind::Session => {}
            }
        }
        Workspace {
            name: name.to_owned(),
            vars,
            profiles,
        }
    }

    /// Builds `(dir, CollectionMeta)` for every collection scope.
    fn build_collection_metas(&self) -> Vec<(PathBuf, CollectionMeta)> {
        self.scopes
            .iter()
            .filter_map(|scope| match &scope.kind {
                EnvScopeKind::Collection { dir } => Some((
                    dir.clone(),
                    CollectionMeta {
                        vars: rows_to_map(&scope.vars),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    /// Whether any workspace/profile scope differs from the snapshot (the manifest
    /// carries both; a change to either requires rewriting `churl.toml`).
    fn manifest_scopes_changed(&self) -> bool {
        let is_manifest =
            |s: &EnvScope| matches!(s.kind, EnvScopeKind::Workspace | EnvScopeKind::Profile);
        let now: Vec<&EnvScope> = self.scopes.iter().filter(|s| is_manifest(s)).collect();
        let was: Vec<&EnvScope> = self.snapshot.iter().filter(|s| is_manifest(s)).collect();
        now != was
    }

    /// Whether the collection scope for `dir` differs from the snapshot.
    fn collection_scope_changed(&self, dir: &Path) -> bool {
        let find = |scopes: &[EnvScope]| -> Option<EnvScope> {
            scopes
                .iter()
                .find(|s| matches!(&s.kind, EnvScopeKind::Collection { dir: d } if d == dir))
                .cloned()
        };
        find(&self.scopes) != find(&self.snapshot)
    }

    // --- Precedence ---

    /// Builds the precedence chain for `name` from the selected scope's point of
    /// view (see the module docs). Highest precedence first; each link carries
    /// whether that scope defines `name` and whether it is the selected scope.
    fn precedence_chain(&self, name: &str) -> Vec<ChainLink> {
        let mut links = Vec::new();
        let sel = &self.scopes[self.selected_scope];

        // cli (rank 0) — only shown when it defines the name.
        if self.cli_vars.contains_key(name) {
            links.push(ChainLink {
                label: "cli".to_owned(),
                defines: true,
                is_selected: false,
            });
        }
        // active profile (rank 1) — always shown when a profile is active (so a
        // non-defining active profile renders the `—` note).
        if let Some(active) = &self.active_profile
            && let Some(pscope) = self
                .scopes
                .iter()
                .find(|s| matches!(s.kind, EnvScopeKind::Profile) && &s.label == active)
        {
            links.push(ChainLink {
                label: format!("profile {active}"),
                defines: pscope.vars.iter().any(|(n, _)| n == name),
                is_selected: std::ptr::eq(pscope, sel),
            });
        }
        // collection (rank 2) — only the selected collection participates (the
        // resolver's collection layer is per-endpoint; we annotate the collection
        // being viewed).
        if let EnvScopeKind::Collection { .. } = sel.kind {
            links.push(ChainLink {
                label: format!("collection {}", sel.label),
                defines: sel.vars.iter().any(|(n, _)| n == name),
                is_selected: true,
            });
        }
        // workspace (rank 3) — shown when it defines the name.
        if let Some(wscope) = self
            .scopes
            .iter()
            .find(|s| matches!(s.kind, EnvScopeKind::Workspace))
            && wscope.vars.iter().any(|(n, _)| n == name)
        {
            links.push(ChainLink {
                label: "workspace".to_owned(),
                defines: true,
                is_selected: matches!(sel.kind, EnvScopeKind::Workspace),
            });
        }
        links
    }

    /// Whether the selected scope is a profile that is not the active one (its
    /// values never resolve — an inactive environment).
    fn selected_is_inactive_profile(&self) -> bool {
        let sel = self.scope();
        matches!(sel.kind, EnvScopeKind::Profile)
            && self.active_profile.as_deref() != Some(sel.label.as_str())
    }

    /// Whether any collection scope defines `name`. Used to qualify a workspace
    /// winner: the collection layer (rank 2) sits above workspace (rank 3), so a
    /// workspace value that a collection also defines is overridden for that
    /// collection's endpoints — a bare ` ✓` would overstate the win. (Profile and
    /// cli outrank collections, so their winners stay a precise ` ✓`.)
    fn defined_in_a_collection(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| {
            matches!(s.kind, EnvScopeKind::Collection { .. })
                && s.vars.iter().any(|(n, _)| n == name)
        })
    }

    /// The inline precedence tag for a row `name` in the selected scope. A
    /// workspace ` ✓` winner becomes ` ✓*` when the same name is also set in a
    /// collection (overridden there per-request — see the footer legend).
    fn row_precedence_tag(&self, name: &str) -> String {
        if name.is_empty() {
            return String::new();
        }
        // The Session group is the highest scope; a defined Session var always
        // wins for a standalone send. Tag it plainly rather than running the
        // (Session-unaware) precedence chain.
        if self.selected_is_session() {
            return " ✓".to_owned();
        }
        if self.selected_is_inactive_profile() {
            return " (inactive)".to_owned();
        }
        let links = self.precedence_chain(name);
        let winner = links.iter().find(|l| l.defines);
        match winner {
            Some(w) if w.is_selected => {
                if matches!(self.scope().kind, EnvScopeKind::Workspace)
                    && self.defined_in_a_collection(name)
                {
                    " ✓*".to_owned()
                } else {
                    " ✓".to_owned()
                }
            }
            Some(w) => format!(" → {}", w.label),
            None => String::new(),
        }
    }

    /// Whether the currently-selected row carries the ` ✓*` collection-override
    /// caveat, so the footer can show the legend.
    fn selected_row_has_collection_caveat(&self) -> bool {
        self.scope()
            .vars
            .get(self.selected_row)
            .is_some_and(|(name, _)| self.row_precedence_tag(name).ends_with('*'))
    }

    /// The full precedence chain string for the selected row (footer).
    fn selected_row_chain(&self) -> Option<String> {
        let scope = self.scope();
        let (name, _) = scope.vars.get(self.selected_row)?;
        if name.is_empty() {
            return None;
        }
        if self.selected_is_session() {
            return Some(format!(
                "{name}: in-memory Session capture — resolves standalone; c clears"
            ));
        }
        if self.selected_is_inactive_profile() {
            return Some(format!(
                "{name}: profile {} is inactive — press x to activate",
                scope.label
            ));
        }
        let links = self.precedence_chain(name);
        let mut winner_seen = false;
        let parts: Vec<String> = links
            .iter()
            .map(|l| {
                if !l.defines {
                    format!("{} —", l.label)
                } else if !winner_seen {
                    winner_seen = true;
                    format!("{} ✓", l.label)
                } else {
                    format!("{} (shadowed)", l.label)
                }
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(format!("{name}: {}", parts.join(" · ")))
        }
    }
}

/// The result of an editor [`save`](EnvEditorState::save).
#[derive(Debug, Clone)]
pub enum EnvSaveResult {
    /// Wrote everything (or nothing needed writing); the new manifest to apply.
    Ok {
        /// The rebuilt workspace manifest (for live-refresh of `app.workspace`).
        workspace: Workspace,
        /// The active profile the editor settled on (apply to the app).
        active_profile: Option<String>,
    },
    /// Refused on a secret violation; nothing was written.
    Refused(String),
    /// An IO error mid-save; the message names what was/wasn't written.
    Failed(String),
}

/// One link in a precedence chain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChainLink {
    label: String,
    defines: bool,
    is_selected: bool,
}

/// A `BTreeMap` → ordered `Vec` of rows (sorted by key, the on-disk order).
fn map_to_rows(map: &BTreeMap<String, String>) -> Vec<(String, String)> {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Ordered rows → `BTreeMap`, dropping rows with an empty (whitespace-only) name.
/// Duplicate names collapse (last wins) — the editor keeps them visible until
/// save so the user sees the collision.
fn rows_to_map(rows: &[(String, String)]) -> BTreeMap<String, String> {
    rows.iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .map(|(name, value)| (name.trim().to_owned(), value.clone()))
        .collect()
}

/// Human-readable list of what a partial save had written before it failed.
fn describe_written(manifest: bool, collections: &[PathBuf]) -> String {
    let mut parts = Vec::new();
    if manifest {
        parts.push("churl.toml".to_owned());
    }
    for dir in collections {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            parts.push(format!("{name}/folder.toml"));
        }
    }
    if parts.is_empty() {
        "nothing".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Renders the environments & variables editor over `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    // Near-full-screen centered modal; clamps gracefully on small terminals.
    let [modal] = Layout::horizontal([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let dirty = if state.is_dirty() { " ●" } else { "" };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" Environments & Variables{dirty} "))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Body (fill) + a two-row footer (precedence chain, then key hints).
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(inner);

    let left_width = inner.width.saturating_sub(2).clamp(1, 28);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)]).areas(body);

    render_scope_list(frame, left, state, theme);
    render_var_rows(frame, right, state, theme);
    render_footer(frame, footer, state, theme);

    // A profile-name prompt or the discard confirm sit on top.
    if let Some(naming) = &state.naming {
        let title = match naming.target {
            ProfileNameTarget::New => "New profile",
            ProfileNameTarget::Rename(_) => "Rename profile",
        };
        prompt::render_prompt(frame, modal, title, &naming.editor, None, theme);
    } else if state.pending_close {
        prompt::render_confirm(
            frame,
            modal,
            "Unsaved changes",
            "Save changes before closing?",
            "s save · d discard · esc stay",
            theme,
        );
    }
}

/// Renders the left scope list, grouped with dim section headers.
fn render_scope_list(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let focused = state.focus == EnvFocus::ScopeList;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let block = Block::bordered()
        .border_style(border)
        .title(" Scopes ")
        .title_style(theme.title);
    let list_area = block.inner(area);
    frame.render_widget(block, area);
    if list_area.width == 0 || list_area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut last_group: Option<&str> = None;
    for (i, scope) in state.scopes.iter().enumerate() {
        let group = match scope.kind {
            EnvScopeKind::Workspace => "WORKSPACE",
            EnvScopeKind::Collection { .. } => "COLLECTIONS",
            EnvScopeKind::Profile => "PROFILES",
            EnvScopeKind::Session => "SESSION (read-only)",
        };
        if last_group != Some(group) {
            if last_group.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(group.to_owned(), theme.auth_mask));
            last_group = Some(group);
        }
        // The active profile carries the `●` marker (M6.5 convention).
        let marker = if matches!(scope.kind, EnvScopeKind::Profile)
            && state.active_profile.as_deref() == Some(scope.label.as_str())
        {
            "● "
        } else {
            "  "
        };
        let label = format!("{marker}{}", scope.label);
        let line = Line::from(label);
        lines.push(if i == state.selected_scope {
            line.style(theme.selection)
        } else {
            line
        });
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Renders the right var-row list for the selected scope.
fn render_var_rows(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let focused = state.focus == EnvFocus::VarRows;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let scope = &state.scopes[state.selected_scope];
    let block = Block::bordered()
        .border_style(border)
        .title(format!(" {} ", scope.label))
        .title_style(theme.title);
    let rows_area = block.inner(area);
    frame.render_widget(block, area);
    if rows_area.width == 0 || rows_area.height == 0 {
        return;
    }

    if scope.vars.is_empty() {
        let empty = if matches!(scope.kind, EnvScopeKind::Session) {
            "(no session captures yet — run a sequence with a Session-target rule)"
        } else {
            "(no variables — press a to add)"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(empty, theme.auth_mask)),
            rows_area,
        );
        return;
    }

    // Align values in a column after the widest name (clamped).
    let name_col = scope
        .vars
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 24);

    let visible = rows_area.height as usize;
    let offset = state.selected_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = scope
        .vars
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, (name, value))| {
            let selected = i == state.selected_row && focused;
            render_var_line(state, theme, i, name, value, name_col, selected)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);
}

/// Builds one `name  value  <precedence>` row line.
fn render_var_line<'a>(
    state: &EnvEditorState,
    theme: &Theme,
    row: usize,
    name: &str,
    value: &str,
    name_col: usize,
    selected: bool,
) -> Line<'a> {
    let editing = state.editing.as_ref().filter(|e| e.row == row);
    let editing_name = editing.map(|e| e.field == EnvField::Name).unwrap_or(false);
    let editing_value = editing.map(|e| e.field == EnvField::Value).unwrap_or(false);

    let name_cell = if editing_name {
        field_with_cursor(editing.unwrap())
    } else {
        pad(name, name_col)
    };

    // An ephemeral peek (drive-test note #3) reveals THIS row's resolved value in
    // place — but only when the reveal is pinned to exactly this scope+row (guarded
    // in `revealed_value`), so a stale reveal can never paint another row's secret.
    let revealed = (row == state.selected_row)
        .then(|| state.revealed_value())
        .flatten();

    // Session captures are ALWAYS masked — a captured token is a secret
    // regardless of its var name (note #6). Elsewhere, secret-named literal values
    // are masked unless a placeholder or being edited. A live peek overrides the
    // mask for its one row only.
    let value_cell = if editing_value {
        field_with_cursor(editing.unwrap())
    } else if let Some(plain) = revealed {
        plain.to_owned()
    } else if !value.is_empty()
        && (matches!(state.scope().kind, EnvScopeKind::Session)
            || (looks_like_secret_name(name) && !is_template_placeholder(value)))
    {
        "••••••".to_owned()
    } else {
        value.to_owned()
    };

    let tag = if editing.is_some() {
        String::new()
    } else {
        state.row_precedence_tag(name)
    };

    let mut spans = vec![
        Span::raw(pad(&name_cell, name_col)),
        Span::raw("  "),
        Span::raw(value_cell),
    ];
    // A small affordance right on the revealed row: it is a secret currently
    // visible, and how to re-mask / that it is ephemeral.
    if revealed.is_some() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "👁 revealed · y copy · any key re-masks",
            theme.status_error,
        ));
    }
    if !tag.is_empty() {
        spans.push(Span::styled(tag, theme.auth_mask));
    }
    let line = Line::from(spans);
    if selected {
        line.style(theme.selection)
    } else {
        line
    }
}

/// Renders an editing field's text with a block cursor at the caret.
fn field_with_cursor(edit: &EnvFieldEdit) -> String {
    let text = edit.editor.text();
    let cursor = edit.editor.cursor();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i == cursor {
            out.push('█');
        }
        out.push(*c);
    }
    if cursor >= chars.len() {
        out.push('█');
    }
    out
}

/// Right-pads `text` to at least `width` display columns (char-count based;
/// good enough for the ASCII-ish var names this aligns).
fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Renders the two-row footer: the selected row's precedence chain, then the
/// live key hints for the focused pane (or the active editor state).
fn render_footer(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let [chain_row, hint_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Line 1: a live message (errors/status) wins over the precedence chain; the
    // `✓*` collection-override legend appends to the chain when relevant.
    let top = if let Some(msg) = &state.message {
        Line::styled(format!(" {msg}"), theme.status_error)
    } else if let Some(chain) = state.selected_row_chain() {
        let legend = if state.selected_row_has_collection_caveat() {
            "   * also set in a collection — overridden there per-request"
        } else {
            ""
        };
        Line::styled(format!(" {chain}{legend}"), theme.auth_mask)
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(top), chain_row);

    // Line 2: key hints, built from the actual handler bindings for this state.
    let dirty = if state.is_dirty() {
        "● unsaved · "
    } else {
        ""
    };
    let hints = if state.pending_close {
        "s save · d discard · esc stay".to_owned()
    } else if state.naming.is_some() {
        "enter commit · esc cancel".to_owned()
    } else if state.editing.is_some() {
        "enter commit · tab name/value · esc cancel".to_owned()
    } else if state.selected_is_session() {
        // Read-only Session group: only view / clear / navigate.
        match state.focus {
            EnvFocus::ScopeList => {
                format!(
                    "{dirty}j/k move · l/enter view (read-only) · c clear session · w save · q close"
                )
            }
            EnvFocus::VarRows => {
                format!(
                    "{dirty}j/k move · p peek · read-only (run-populated, masked) · h scopes · q close"
                )
            }
        }
    } else {
        match state.focus {
            EnvFocus::ScopeList => format!(
                "{dirty}j/k move · l/enter edit vars · n new profile · r rename · d delete · x activate · w save · q close"
            ),
            EnvFocus::VarRows => format!(
                "{dirty}j/k move · a add · enter value · r name · d delete · p peek · h scopes · w save · q close"
            ),
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {hints}"), theme.statusline)),
        hint_row,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_str(state: &mut EnvEditorState, s: &str) {
        for c in s.chars() {
            state.handle_key(ch(c));
        }
    }

    /// A fixture editor: workspace {base_url}, one collection {page_size}, one
    /// profile "dev" {host}.
    fn fixture() -> EnvEditorState {
        EnvEditorState {
            scopes: vec![
                EnvScope {
                    kind: EnvScopeKind::Workspace,
                    label: "Workspace".into(),
                    vars: vec![("base_url".into(), "https://api.example.com".into())],
                },
                EnvScope {
                    kind: EnvScopeKind::Collection {
                        dir: PathBuf::from("/ws/users"),
                    },
                    label: "users".into(),
                    vars: vec![("page_size".into(), "50".into())],
                },
                EnvScope {
                    kind: EnvScopeKind::Profile,
                    label: "dev".into(),
                    vars: vec![("host".into(), "dev.example.com".into())],
                },
            ],
            snapshot: vec![],
            selected_scope: 0,
            focus: EnvFocus::ScopeList,
            selected_row: 0,
            editing: None,
            naming: None,
            message: None,
            reveal: None,
            pending_close: false,
            active_profile: None,
            snapshot_active_profile: None,
            cli_vars: BTreeMap::new(),
        }
        .with_snapshot()
    }

    impl EnvEditorState {
        fn with_snapshot(mut self) -> Self {
            self.snapshot = self.scopes.clone();
            self.snapshot_active_profile = self.active_profile.clone();
            self
        }
    }

    #[test]
    fn open_populates_scopes_in_order() {
        let s = fixture();
        assert_eq!(s.scopes.len(), 3);
        assert!(matches!(s.scopes[0].kind, EnvScopeKind::Workspace));
        assert!(matches!(s.scopes[1].kind, EnvScopeKind::Collection { .. }));
        assert!(matches!(s.scopes[2].kind, EnvScopeKind::Profile));
        assert!(!s.is_dirty());
    }

    #[test]
    fn focus_moves_between_panes() {
        let mut s = fixture();
        assert_eq!(s.focus, EnvFocus::ScopeList);
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.focus, EnvFocus::VarRows);
        s.handle_key(ch('h'));
        assert_eq!(s.focus, EnvFocus::ScopeList);
        // Selecting the collection scope, then entering rows.
        s.handle_key(ch('j'));
        assert_eq!(s.selected_scope, 1);
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.focus, EnvFocus::VarRows);
    }

    #[test]
    fn add_edit_and_delete_a_var() {
        let mut s = fixture();
        s.handle_key(key(KeyCode::Tab)); // into workspace rows
        s.handle_key(ch('a')); // add row → Name edit
        assert!(s.editing.is_some());
        type_str(&mut s, "token_url");
        s.handle_key(key(KeyCode::Enter)); // commit name → auto Value edit
        assert!(s.editing.as_ref().unwrap().field == EnvField::Value);
        type_str(&mut s, "https://auth");
        s.handle_key(key(KeyCode::Enter));
        assert!(s.editing.is_none());
        let ws = &s.scopes[0].vars;
        assert!(
            ws.iter()
                .any(|(n, v)| n == "token_url" && v == "https://auth")
        );
        assert!(s.is_dirty());
        // Delete it.
        s.handle_key(ch('d'));
        assert!(!s.scopes[0].vars.iter().any(|(n, _)| n == "token_url"));
    }

    #[test]
    fn cancel_new_row_drops_empty_placeholder() {
        let mut s = fixture();
        s.handle_key(key(KeyCode::Tab));
        let before = s.scopes[0].vars.len();
        s.handle_key(ch('a'));
        s.handle_key(key(KeyCode::Esc)); // cancel the fresh, empty row
        assert_eq!(s.scopes[0].vars.len(), before, "empty new row dropped");
    }

    #[test]
    fn edit_value_of_existing_var() {
        let mut s = fixture();
        s.handle_key(key(KeyCode::Tab));
        s.handle_key(key(KeyCode::Enter)); // edit value of base_url
        // Clear then retype.
        for _ in 0..40 {
            s.handle_key(key(KeyCode::Backspace));
        }
        type_str(&mut s, "https://changed");
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.scopes[0].vars[0].1, "https://changed");
    }

    #[test]
    fn add_rename_and_delete_a_profile() {
        let mut s = fixture();
        // New profile from the scope list.
        s.handle_key(ch('n'));
        type_str(&mut s, "prod");
        s.handle_key(key(KeyCode::Enter));
        assert!(
            s.scopes
                .iter()
                .any(|sc| matches!(sc.kind, EnvScopeKind::Profile) && sc.label == "prod")
        );
        assert_eq!(s.focus, EnvFocus::VarRows, "new profile focuses its rows");
        // Back to the scope list, land on the new profile, rename it.
        s.handle_key(ch('h'));
        // selected_scope is the new profile (last). Rename.
        s.handle_key(ch('r'));
        assert!(s.naming.is_some());
        for _ in 0..8 {
            s.handle_key(key(KeyCode::Backspace));
        }
        type_str(&mut s, "prod2");
        s.handle_key(key(KeyCode::Enter));
        assert!(s.scopes.iter().any(|sc| sc.label == "prod2"));
        assert!(!s.scopes.iter().any(|sc| sc.label == "prod"));
        // Delete it.
        s.handle_key(ch('d'));
        assert!(!s.scopes.iter().any(|sc| sc.label == "prod2"));
    }

    #[test]
    fn rename_on_workspace_scope_is_refused_with_message() {
        let mut s = fixture();
        s.handle_key(ch('r')); // workspace scope selected
        assert!(s.naming.is_none());
        assert!(s.message.as_deref().unwrap().contains("only profiles"));
    }

    #[test]
    fn delete_on_collection_scope_is_refused() {
        let mut s = fixture();
        s.handle_key(ch('j')); // collection
        s.handle_key(ch('d'));
        assert_eq!(s.scopes.len(), 3, "collection scope not deleted");
        assert!(s.message.as_deref().unwrap().contains("explorer"));
    }

    #[test]
    fn dirty_close_requests_confirm_then_discards() {
        let mut s = fixture();
        s.handle_key(key(KeyCode::Tab));
        s.handle_key(ch('a'));
        type_str(&mut s, "k");
        s.handle_key(key(KeyCode::Enter));
        type_str(&mut s, "v");
        s.handle_key(key(KeyCode::Enter));
        assert!(s.is_dirty());
        // q → confirm, not close.
        let out = s.handle_key(ch('q'));
        assert_eq!(out, EnvKeyOutcome::Consumed);
        assert!(s.pending_close);
        // Esc → stay.
        s.handle_key(key(KeyCode::Esc));
        assert!(!s.pending_close);
        // q again → confirm, then d → close.
        s.handle_key(ch('q'));
        let out = s.handle_key(ch('d'));
        assert_eq!(out, EnvKeyOutcome::Close);
    }

    #[test]
    fn clean_close_is_immediate() {
        let mut s = fixture();
        let out = s.handle_key(ch('q'));
        assert_eq!(out, EnvKeyOutcome::Close);
        assert!(!s.pending_close);
    }

    #[test]
    fn save_refuses_literal_secret() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let mut s = fixture();
        // Add a secret-named literal to the workspace.
        s.scopes[0].vars.push(("api_token".into(), "leaked".into()));
        let result = s.save(dir.path(), "demo");
        let EnvSaveResult::Refused(msg) = &result else {
            panic!("expected Refused, got {result:?}");
        };
        // D1: the interim message names the offending var and signals it's
        // pre-existing (the full grandfather+warn behavior is R3).
        assert!(
            msg.contains("api_token"),
            "refusal names the offending var: {msg}"
        );
        assert!(
            msg.contains("pre-existing"),
            "refusal signals the value is pre-existing: {msg}"
        );
        // The same message is surfaced to the user via `self.message`.
        assert_eq!(s.message.as_deref(), Some(msg.as_str()));
        // Nothing written: the manifest still has only its original line.
        let text = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
        assert!(
            !text.contains("api_token"),
            "refused save wrote nothing:\n{text}"
        );
    }

    #[test]
    fn save_accepts_secret_named_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let mut s = fixture();
        s.scopes[0]
            .vars
            .push(("api_token".into(), "{{api_token}}".into()));
        let result = s.save(dir.path(), "demo");
        assert!(matches!(result, EnvSaveResult::Ok { .. }));
        let text = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
        assert!(text.contains("api_token"));
        assert!(!s.is_dirty(), "snapshot refreshed after save");
    }

    #[test]
    fn save_writes_all_three_scopes_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = dir.path().join("users");
        std::fs::create_dir(&coll).unwrap();
        let mut s = EnvEditorState {
            scopes: vec![
                EnvScope {
                    kind: EnvScopeKind::Workspace,
                    label: "Workspace".into(),
                    vars: vec![("base_url".into(), "https://api".into())],
                },
                EnvScope {
                    kind: EnvScopeKind::Collection { dir: coll.clone() },
                    label: "users".into(),
                    vars: vec![("page_size".into(), "50".into())],
                },
                EnvScope {
                    kind: EnvScopeKind::Profile,
                    label: "dev".into(),
                    vars: vec![("host".into(), "dev.example".into())],
                },
            ],
            snapshot: vec![],
            selected_scope: 0,
            focus: EnvFocus::ScopeList,
            selected_row: 0,
            editing: None,
            naming: None,
            message: None,
            reveal: None,
            pending_close: false,
            active_profile: None,
            snapshot_active_profile: None,
            cli_vars: BTreeMap::new(),
        }
        .with_snapshot();
        // Mutate one var in each scope.
        s.scopes[0].vars[0].1 = "https://api2".into();
        s.scopes[1].vars[0].1 = "100".into();
        s.scopes[2].vars[0].1 = "dev2.example".into();

        let result = s.save(dir.path(), "demo");
        let EnvSaveResult::Ok { workspace, .. } = result else {
            panic!("expected Ok");
        };
        assert_eq!(
            workspace.vars.get("base_url").map(String::as_str),
            Some("https://api2")
        );
        assert_eq!(
            workspace.profiles[0].vars.get("host").map(String::as_str),
            Some("dev2.example")
        );
        // Re-read from disk to prove persistence.
        let ws = load_collection_meta(&coll).unwrap();
        assert_eq!(ws.vars.get("page_size").map(String::as_str), Some("100"));
    }

    #[test]
    fn precedence_marks_winner_and_shadow() {
        let mut s = fixture();
        // Give the collection a var that also exists in workspace.
        s.scopes[1]
            .vars
            .push(("base_url".into(), "https://coll".into()));
        // Activate dev; add base_url to dev too.
        s.active_profile = Some("dev".into());
        s.scopes[2]
            .vars
            .push(("base_url".into(), "https://dev".into()));

        // Viewing the workspace scope: base_url is shadowed by the active profile.
        s.selected_scope = 0;
        s.selected_row = 0;
        assert!(s.row_precedence_tag("base_url").contains("→ profile dev"));

        // Viewing the profile scope (active): base_url wins.
        s.selected_scope = 2;
        // base_url is the 2nd row in dev now (host, base_url).
        assert_eq!(s.row_precedence_tag("base_url"), " ✓");

        // Viewing the collection scope: profile shadows the collection's base_url.
        s.selected_scope = 1;
        assert!(s.row_precedence_tag("base_url").contains("→ profile dev"));
        // page_size is unique to the collection → it wins.
        assert_eq!(s.row_precedence_tag("page_size"), " ✓");
    }

    #[test]
    fn inactive_profile_rows_marked_inactive() {
        let mut s = fixture();
        s.active_profile = None; // dev is not active
        s.selected_scope = 2;
        assert_eq!(s.row_precedence_tag("host"), " (inactive)");
    }

    #[test]
    fn set_active_profile_toggles() {
        let mut s = fixture();
        s.selected_scope = 2; // dev
        s.set_active_profile();
        assert_eq!(s.active_profile.as_deref(), Some("dev"));
        s.set_active_profile();
        assert_eq!(s.active_profile, None);
    }

    #[test]
    fn empty_var_names_dropped_on_save_reconcile() {
        let s = fixture();
        let mut rows = s.scopes[0].vars.clone();
        rows.push(("  ".into(), "orphan".into()));
        let map = rows_to_map(&rows);
        assert!(
            !map.values().any(|v| v == "orphan"),
            "blank-named row dropped"
        );
    }

    // --- Fix-round (2026-07-08 review) regression tests. ---

    /// A fixture with a read-only Session group holding one capture.
    fn fixture_with_session() -> EnvEditorState {
        let mut s = fixture();
        s.scopes.push(EnvScope {
            kind: EnvScopeKind::Session,
            label: "Session".into(),
            vars: vec![("token".into(), "T-abc-123".into())],
        });
        s.snapshot = s.scopes.clone();
        s
    }

    fn render_to_text(state: &EnvEditorState) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| render(frame, frame.area(), state, &theme))
            .unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn session_group_shows_masked_values() {
        // Note #6: the Session group renders its captures masked (a token is a
        // secret regardless of its var name), and the raw value never appears.
        let mut s = fixture_with_session();
        // Select the Session scope (last) and enter its rows.
        s.selected_scope = s.scopes.len() - 1;
        s.focus = EnvFocus::VarRows;
        let out = render_to_text(&s);
        assert!(
            out.contains("SESSION"),
            "session group header shown:\n{out}"
        );
        assert!(out.contains("token"), "the var name is visible");
        assert!(out.contains("••••••"), "the value is masked:\n{out}");
        assert!(
            !out.contains("T-abc-123"),
            "the raw session value never renders:\n{out}"
        );
    }

    #[test]
    fn session_group_is_read_only() {
        let mut s = fixture_with_session();
        s.selected_scope = s.scopes.len() - 1;
        // Editing keys in the rows are inert.
        s.focus = EnvFocus::VarRows;
        s.handle_key(ch('a'));
        assert!(s.editing.is_none(), "cannot add a session row");
        s.handle_key(key(KeyCode::Enter));
        assert!(s.editing.is_none(), "cannot edit a session value");
        // Scope-list mutations are inert too.
        s.focus = EnvFocus::ScopeList;
        let before = s.scopes.len();
        s.handle_key(ch('d'));
        assert_eq!(s.scopes.len(), before, "cannot delete the session group");
        assert!(!s.is_dirty(), "the read-only group never dirties");
    }

    #[test]
    fn session_clear_key_emits_clear_outcome() {
        let mut s = fixture_with_session();
        s.selected_scope = s.scopes.len() - 1;
        s.focus = EnvFocus::ScopeList;
        assert_eq!(s.handle_key(ch('c')), EnvKeyOutcome::ClearSession);
        // `c` on a non-session scope is inert (no clear).
        s.selected_scope = 0;
        assert_ne!(s.handle_key(ch('c')), EnvKeyOutcome::ClearSession);
    }

    #[test]
    fn set_session_vars_replaces_rows_without_dirtying() {
        let mut s = fixture_with_session();
        s.set_session_vars(&BTreeMap::new());
        let session = s
            .scopes
            .iter()
            .find(|sc| matches!(sc.kind, EnvScopeKind::Session))
            .unwrap();
        assert!(session.vars.is_empty(), "cleared to empty");
        assert!(
            !s.is_dirty(),
            "clearing the read-only group is not a dirty edit"
        );
    }

    #[test]
    fn esc_reverts_an_existing_field_edit() {
        // Fix 1: Esc must restore the pre-edit value (the live mirror is reverted)
        // and leave the editor clean.
        let mut s = fixture();
        s.handle_key(key(KeyCode::Tab)); // into workspace rows (base_url)
        let original = s.scopes[0].vars[0].1.clone();
        s.handle_key(key(KeyCode::Enter)); // edit base_url value
        type_str(&mut s, "XYZ");
        assert!(s.is_dirty(), "typing live-mirrors → dirty mid-edit");
        s.handle_key(key(KeyCode::Esc)); // cancel
        assert_eq!(s.scopes[0].vars[0].1, original, "value reverted");
        assert!(!s.is_dirty(), "cancel leaves the editor clean");
    }

    #[test]
    fn winner_marker_flags_collection_override() {
        // Fix 2: a workspace var also defined in a collection shows ✓* (not a bare
        // ✓), because the collection value wins for that collection's endpoints.
        let mut s = fixture();
        // `page_size` lives in the collection; also add it to the workspace.
        s.scopes[0].vars.push(("page_size".into(), "10".into()));
        s.selected_scope = 0; // workspace
        s.selected_row = s.scopes[0].vars.len() - 1; // the page_size row
        assert_eq!(s.row_precedence_tag("page_size"), " ✓*");
        assert!(
            s.selected_row_has_collection_caveat(),
            "selected ✓* row shows the footer legend"
        );
        // A workspace var that no collection defines stays a bare ✓.
        assert_eq!(s.row_precedence_tag("base_url"), " ✓");
        // In the collection scope view the same var is precise (bare ✓).
        s.selected_scope = 1;
        assert_eq!(s.row_precedence_tag("page_size"), " ✓");
    }

    #[test]
    fn save_refuses_duplicate_var_names() {
        // Fix 3: two rows with the same name refuse the whole save (no silent
        // last-wins collapse), naming the scope + var; the file is untouched.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let before = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
        let mut s = fixture();
        s.scopes[0]
            .vars
            .push(("base_url".into(), "https://dup".into()));
        let result = s.save(dir.path(), "demo");
        match result {
            EnvSaveResult::Refused(msg) => {
                assert!(msg.contains("duplicate var name"), "{msg}");
                assert!(msg.contains("base_url"), "{msg}");
                assert!(msg.contains("Workspace"), "{msg}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        let after = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
        assert_eq!(before, after, "refused save writes nothing");
    }

    #[test]
    fn activating_profile_is_dirty_discard_reverts_save_commits() {
        // Fix 4: `x` folds into dirtiness; discard reverts active_profile; save
        // updates the snapshot so it clears.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let mut s = fixture();
        assert!(!s.is_dirty());
        s.selected_scope = 2; // dev
        s.set_active_profile(); // activate dev
        assert_eq!(s.active_profile.as_deref(), Some("dev"));
        assert!(s.is_dirty(), "activating a profile marks dirty");

        // Discard (via the confirm) reverts the active profile.
        let out = s.handle_key(ch('q')); // dirty → confirm
        assert_eq!(out, EnvKeyOutcome::Consumed);
        assert!(s.pending_close);
        let out = s.handle_key(ch('d')); // discard
        assert_eq!(out, EnvKeyOutcome::Close);
        assert_eq!(s.active_profile, None, "discard reverts active profile");

        // Re-activate and save: the snapshot updates so dirty clears.
        s.pending_close = false;
        s.set_active_profile();
        assert!(s.is_dirty());
        let result = s.save(dir.path(), "demo");
        assert!(matches!(result, EnvSaveResult::Ok { .. }));
        assert!(!s.is_dirty(), "save commits the active-profile change");
    }

    // --- Ephemeral peek + copy (drive-test note #3). ---

    /// Select the Session scope's first row (a masked capture) with VarRows focus.
    fn on_session_row(s: &mut EnvEditorState) {
        s.selected_scope = s.scopes.len() - 1;
        s.focus = EnvFocus::VarRows;
        s.selected_row = 0;
    }

    #[test]
    fn peek_key_on_masked_row_requests_reveal() {
        // `p` on a masked row asks the app to resolve it (RevealRow). The editor
        // does NOT reveal on its own — it holds no resolver.
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        assert_eq!(s.handle_key(ch('p')), EnvKeyOutcome::RevealRow);
        assert!(
            s.revealed_value().is_none(),
            "no reveal until the app answers"
        );
    }

    #[test]
    fn peek_on_visible_row_is_a_no_op_hint() {
        // `p` on a plainly-visible (non-secret) row reveals nothing.
        let mut s = fixture(); // workspace base_url is not a secret
        s.focus = EnvFocus::VarRows;
        s.selected_row = 0;
        assert_eq!(s.handle_key(ch('p')), EnvKeyOutcome::Consumed);
        assert!(s.message.as_deref().unwrap().contains("not masked"));
        assert!(s.revealed_value().is_none());
    }

    #[test]
    fn reveal_shows_the_resolved_value_in_place() {
        // The app hands the resolved plaintext to set_reveal; the row then renders
        // it verbatim (not the mask) with the "revealed" affordance.
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        assert_eq!(s.handle_key(ch('p')), EnvKeyOutcome::RevealRow);
        s.set_reveal("T-abc-123".to_owned());
        assert_eq!(s.revealed_value(), Some("T-abc-123"));
        let out = render_to_text(&s);
        assert!(out.contains("T-abc-123"), "resolved value visible:\n{out}");
        assert!(out.contains("revealed"), "affordance shown:\n{out}");
    }

    #[test]
    fn reveal_remasks_on_row_move() {
        let mut s = fixture_with_session();
        // Two session rows so a move lands on a different one.
        let last = s.scopes.len() - 1;
        s.scopes[last].vars.push(("other".into(), "V-2".into()));
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        assert!(s.revealed_value().is_some());
        s.handle_key(ch('j')); // move to another row
        assert!(s.revealed_value().is_none(), "moving off the row re-masks");
        let out = render_to_text(&s);
        assert!(
            !out.contains("T-abc-123"),
            "no plaintext after move:\n{out}"
        );
    }

    #[test]
    fn reveal_remasks_on_mode_change() {
        // Leaving the rows pane (h → scope list) re-masks.
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        s.handle_key(ch('h')); // focus → ScopeList (a mode/pane change)
        assert!(s.revealed_value().is_none(), "changing pane re-masks");
    }

    #[test]
    fn reveal_remasks_on_timeout() {
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        // Back-date the reveal past the expiry and tick.
        if let Some(r) = s.reveal.as_mut() {
            r.revealed_at = Instant::now() - std::time::Duration::from_secs(REVEAL_EXPIRE_SECS + 1);
        }
        assert!(s.expire_reveal(), "expired reveal is cleared");
        assert!(s.revealed_value().is_none());
        // A fresh reveal does not expire.
        s.set_reveal("T-abc-123".to_owned());
        assert!(!s.expire_reveal());
        assert!(s.revealed_value().is_some());
    }

    #[test]
    fn only_one_row_revealed_at_a_time() {
        let mut s = fixture_with_session();
        let last = s.scopes.len() - 1;
        s.scopes[last].vars.push(("other".into(), "V-2".into()));
        on_session_row(&mut s);
        s.set_reveal("first".to_owned());
        // Move + peek + reveal the second row: the first reveal is gone.
        s.handle_key(ch('j'));
        s.handle_key(ch('p'));
        s.set_reveal("second".to_owned());
        assert_eq!(s.revealed_value(), Some("second"));
        // Only the currently-selected (second) row is revealed.
        s.selected_row = 0;
        assert!(s.revealed_value().is_none(), "row 0 is re-masked");
    }

    #[test]
    fn y_while_revealed_emits_copy_revealed() {
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        assert_eq!(s.handle_key(ch('y')), EnvKeyOutcome::CopyRevealed);
        // The reveal survives the copy so the app can read the value back.
        assert_eq!(s.revealed_value(), Some("T-abc-123"));
    }

    #[test]
    fn y_without_reveal_is_a_no_op_hint() {
        let mut s = fixture_with_session();
        on_session_row(&mut s);
        assert_eq!(s.handle_key(ch('y')), EnvKeyOutcome::Consumed);
        assert!(s.message.as_deref().unwrap().contains("nothing revealed"));
    }

    #[test]
    fn peek_never_makes_the_session_row_editable() {
        // The core security invariant: a peek reveals but NEVER enters edit mode,
        // and the Session group stays read-only under reveal.
        let mut s = fixture_with_session();
        let before = s.scopes.clone();
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        assert!(s.editing.is_none(), "peek does not open an editor");
        // Edit/add/delete keys are still swallowed with the read-only message.
        s.handle_key(ch('i'));
        assert!(s.editing.is_none(), "still not editable");
        s.handle_key(ch('a'));
        assert!(s.editing.is_none(), "cannot add under reveal");
        s.handle_key(ch('d'));
        assert_eq!(s.scopes, before, "no row mutated under reveal");
        assert!(
            !s.is_dirty(),
            "the read-only group never dirties under reveal"
        );
    }

    #[test]
    fn reveal_lives_only_in_view_state_never_in_saved_rows() {
        // No-persistence proof at the unit seam: the reveal plaintext lives ONLY in
        // the transient `reveal` field. The scope rows (what save writes) and the
        // snapshot are untouched by a peek, and clearing the reveal drops the
        // plaintext entirely.
        let mut s = fixture_with_session();
        let rows_before = s.scopes.clone();
        on_session_row(&mut s);
        s.handle_key(ch('p'));
        s.set_reveal("T-abc-123".to_owned());
        assert_eq!(s.scopes, rows_before, "peek never writes into the rows");
        // The Session row still holds its stored value; the reveal is separate.
        s.clear_reveal();
        assert!(
            s.reveal.is_none(),
            "clearing drops the plaintext from view state"
        );
        assert!(s.revealed_value().is_none());
    }
}
