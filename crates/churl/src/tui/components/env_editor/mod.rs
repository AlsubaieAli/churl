//! The environments & variables editor: a split-view modal for editing the
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

use churl_core::config::{is_template_placeholder, looks_like_secret_name};
use churl_core::model::{CollectionMeta, Profile, Workspace};
use churl_core::persistence::{
    OpenWorkspace, PersistenceError, load_collection_meta, load_workspace_manifest,
    save_collection_meta_checked, save_workspace_manifest_checked,
};
use churl_core::secrets::{SecretPolicy, decide, scan_collection, scan_workspace};

use super::line_editor::LineEditor;

mod edit;
mod render;

// Re-export the render entry point so `env_editor::render` (called by `app`)
// resolves unchanged after the split. `render` was and stays `pub`.
pub use render::render;

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
    /// The in-memory Session captures for the current workspace. A
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
    /// Clear the current workspace's in-memory Session captures. The app
    /// empties its store, then the editor rebuilds the Session group's rows.
    ClearSession,
    /// Ephemeral peek: the user pressed the reveal key on a
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
    /// Copy the selected NON-masked row's value directly (`y` on a plainly-visible
    /// row). No peek is needed for a value that is already
    /// on screen: masked/secret rows keep the reveal-first gate ([`CopyRevealed`]),
    /// visible rows copy outright. The app reads [`EnvEditorState::selected_row_value`]
    /// and routes it through the same clipboard path.
    CopyValue,
}

/// Default lifetime of an ephemeral secret peek, in seconds — deliberately short
/// so a revealed secret does not linger on screen. Checked on the app's existing
/// 250 ms tick (mirrors [`super::message::MESSAGE_EXPIRE_SECS`]).
pub const REVEAL_EXPIRE_SECS: u64 = 6;

/// An active ephemeral peek: exactly one masked row's
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
    /// The active ephemeral secret peek, if any. At most one
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

        // The read-only Session group: the current workspace's in-memory
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

    /// Whether the selected scope is the read-only Session group.
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

    /// The raw value of the selected row, verbatim as it renders on a NON-masked
    /// row. Used by the app for the direct `y` copy of a plainly-visible value
    /// — it never resolves templates, so what copies is
    /// exactly what the row shows. `None` when there is no selected row.
    pub fn selected_row_value(&self) -> Option<&str> {
        self.scope()
            .vars
            .get(self.selected_row)
            .map(|(_, v)| v.as_str())
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

    // --- Save ---

    /// Reconciles the working scopes into a [`Workspace`] + collection metas,
    /// classifies secret findings against the on-disk baseline under `policy`, and
    /// writes the changed targets (format-preserving, deletion-pruning). Refuses
    /// (writes nothing) only when the save would *newly author* a name-anchored
    /// literal secret under strict; pre-existing (grandfathered) and value-only
    /// findings save with a warning. On success, refreshes the dirty snapshot and
    /// returns the new manifest for the app to apply live.
    pub fn save(
        &mut self,
        root: &Path,
        workspace_name: &str,
        policy: SecretPolicy,
    ) -> EnvSaveResult {
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

        // Scan the content being saved and the on-disk baseline with identical
        // location strings so novelty compares correctly (a pre-existing literal
        // grandfathers). Collection findings are prefixed by the collection dir
        // name to disambiguate `vars.<x>` across collections.
        let mut new_findings = scan_workspace(&workspace);
        let mut baseline_findings = load_workspace_manifest(root)
            .map(|ws| scan_workspace(&ws))
            .unwrap_or_default();
        for (dir, meta) in &collections {
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<collection>");
            for mut finding in scan_collection(meta) {
                finding.location = format!("{name}.{}", finding.location);
                new_findings.push(finding);
            }
            for mut finding in load_collection_meta(dir)
                .map(|m| scan_collection(&m))
                .unwrap_or_default()
            {
                finding.location = format!("{name}.{}", finding.location);
                baseline_findings.push(finding);
            }
        }

        let decision = decide(&new_findings, &baseline_findings, policy);
        if decision.is_refused() {
            let msg = format!(
                "not saved: new literal secret(s) ({}) — move them to env or use {{{{var}}}}",
                decision.refusal_locations().join(", ")
            );
            self.message = Some(msg.clone());
            return EnvSaveResult::Refused(msg);
        }

        // Write the manifest only when workspace/profile scopes changed.
        let manifest_changed = self.manifest_scopes_changed();
        if manifest_changed
            && let Err(err) = save_workspace_manifest_checked(root, &workspace, policy)
        {
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
            if let Err(err) = save_collection_meta_checked(dir, meta, policy) {
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
            warnings: decision.warning_locations(),
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
    /// `warnings` is non-empty when the save proceeded but carried grandfathered
    /// or value-only secret findings (drives the `!` marker / warning text).
    Ok {
        /// The rebuilt workspace manifest (for live-refresh of `app.workspace`).
        workspace: Workspace,
        /// The active profile the editor settled on (apply to the app).
        active_profile: Option<String>,
        /// Secret warning locations (grandfathered / value-only). Empty = clean.
        warnings: Vec<String>,
    },
    /// Refused on a newly-authored name-anchored secret; nothing was written.
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

#[cfg(test)]
mod tests;
