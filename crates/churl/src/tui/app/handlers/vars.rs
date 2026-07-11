//! Variable-scope + template-resolver plumbing extracted from `app.rs` (M7.11).
//! Grandchild module of `app`, so `impl App` here keeps full access to `App`'s
//! private fields and methods without any visibility widening — see DECISIONS.md,
//! "Module boundaries". Every method carries `pub(in crate::tui::app)` because it
//! is called from the parent module and/or sibling handler clusters (its exact
//! original scope as a bare-private `app` method).

use super::super::*;

impl App {
    /// The variables of the active profile, empty when none is set or it has no
    /// vars (an already-validated profile name that no longer resolves also
    /// yields empty).
    pub(in crate::tui::app) fn profile_vars(&self) -> BTreeMap<String, String> {
        let Some(name) = &self.active_profile else {
            return BTreeMap::new();
        };
        self.workspace
            .as_ref()
            .and_then(|ws| ws.manifest().profiles.iter().find(|p| &p.name == name))
            .map(|p| p.vars.clone())
            .unwrap_or_default()
    }

    /// The workspace-level `[vars]`, empty when there is no workspace.
    pub(in crate::tui::app) fn workspace_vars(&self) -> BTreeMap<String, String> {
        self.workspace
            .as_ref()
            .map(|ws| ws.manifest().vars.clone())
            .unwrap_or_default()
    }

    /// The in-memory Session store key for the current workspace: its canonical
    /// root path. `None` when no workspace is open (nothing to key on).
    pub(in crate::tui::app) fn session_key(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|ws| canonical_path(ws.root()))
    }

    /// The current workspace's in-memory Session captures (note #6), empty when
    /// none or no workspace. The highest resolver scope for a standalone send and
    /// (threaded through [`RunScopes`]) for a sequence run.
    pub(in crate::tui::app) fn session_vars(&self) -> BTreeMap<String, String> {
        self.session_key()
            .and_then(|key| self.session_vars.get(&key).cloned())
            .unwrap_or_default()
    }

    /// Writes `name → value` into the current workspace's Session store,
    /// creating the workspace entry on first write and overwriting an existing
    /// name (a re-login refreshes the token). In-memory only — never persisted.
    /// No-op when no workspace is open.
    pub(in crate::tui::app) fn write_session_var(&mut self, name: String, value: String) {
        let Some(key) = self.session_key() else {
            return;
        };
        self.session_vars
            .entry(key)
            .or_default()
            .insert(name, value);
    }

    /// Clears the current workspace's Session captures (the env-editor Session
    /// group's clear action). Returns whether anything was cleared.
    pub(in crate::tui::app) fn clear_session_vars(&mut self) -> bool {
        let Some(key) = self.session_key() else {
            return false;
        };
        match self.session_vars.get_mut(&key) {
            Some(map) if !map.is_empty() => {
                map.clear();
                true
            }
            _ => false,
        }
    }

    /// Builds the template [`Resolver`] for a send of `selected`, in precedence
    /// order: in-memory Session captures → cli `--var` → active profile → the
    /// endpoint's collection `folder.toml` vars → workspace `[vars]` → process env
    /// (implicit). The Session scope (note #6) sits at the top so a standalone
    /// request using `{{token}}` resolves a value captured by an earlier sequence
    /// run; it is empty until a Session-target rule writes into it.
    pub(in crate::tui::app) fn build_resolver(&mut self, selected: &SelectedEndpoint) -> Resolver {
        let collection_vars = self.explorer.collection_vars(selected.collection);
        Resolver::new(vec![
            Scope::new("session", self.session_vars()),
            Scope::new("cli", self.cli_vars.clone()),
            Scope::new("profile", self.profile_vars()),
            Scope::new("collection", collection_vars),
            Scope::new("workspace", self.workspace_vars()),
        ])
    }

    /// Builds the resolver used by the env-editor's ephemeral peek (drive-test
    /// note #3). It mirrors [`build_resolver`] but omits the per-endpoint
    /// `collection` scope — the env editor is not tied to a loaded endpoint, so
    /// there is no single collection to consult (a collection var resolves
    /// per-request, not globally). Session captures still sit highest, so a peeked
    /// `{{token}}` reveals what a standalone send would use. The resolved value is
    /// returned by value and never stored — the caller hands it straight to the
    /// editor's transient reveal state.
    pub(in crate::tui::app) fn build_env_resolver(&self) -> Resolver {
        Resolver::new(vec![
            Scope::new("session", self.session_vars()),
            Scope::new("cli", self.cli_vars.clone()),
            Scope::new("profile", self.profile_vars()),
            Scope::new("workspace", self.workspace_vars()),
        ])
    }
}
