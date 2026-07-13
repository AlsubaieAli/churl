use super::*;
use crate::tui::theme::Theme;
use churl_core::secrets::SecretPolicy;
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
fn g_and_shift_g_jump_scope_list_top_and_bottom() {
    // Vim `g`/`G` (aliased Home/End) jump the scope list
    // to first/last, matching the single-`g` convention the runners use.
    let mut s = fixture();
    assert_eq!(s.selected_scope, 0);
    s.handle_key(ch('G'));
    assert_eq!(s.selected_scope, s.scopes.len() - 1, "G → last scope");
    assert_eq!(s.selected_row, 0, "jumping scope resets the row");
    s.handle_key(ch('g'));
    assert_eq!(s.selected_scope, 0, "g → first scope");
    // Home/End are aliases.
    s.handle_key(key(KeyCode::End));
    assert_eq!(s.selected_scope, s.scopes.len() - 1);
    s.handle_key(key(KeyCode::Home));
    assert_eq!(s.selected_scope, 0);
}

#[test]
fn g_and_shift_g_jump_var_rows_top_and_bottom() {
    // A scope with several rows, focused into the var-row list.
    let mut s = EnvEditorState {
        scopes: vec![EnvScope {
            kind: EnvScopeKind::Workspace,
            label: "Workspace".into(),
            vars: vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("c".into(), "3".into()),
                ("d".into(), "4".into()),
            ],
        }],
        snapshot: vec![],
        selected_scope: 0,
        focus: EnvFocus::VarRows,
        selected_row: 0,
        editing: None,
        naming: None,
        message: None,
        pending_close: false,
        active_profile: None,
        snapshot_active_profile: None,
        cli_vars: BTreeMap::new(),
        reveal: None,
    }
    .with_snapshot();
    s.handle_key(ch('G'));
    assert_eq!(s.selected_row, 3, "G → last row");
    s.handle_key(ch('g'));
    assert_eq!(s.selected_row, 0, "g → first row");
    // Home/End aliases too.
    s.handle_key(key(KeyCode::End));
    assert_eq!(s.selected_row, 3);
    s.handle_key(key(KeyCode::Home));
    assert_eq!(s.selected_row, 0);
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
fn save_refuses_new_literal_secret() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let mut s = fixture();
    // Add a secret-named literal absent from the on-disk baseline → NEW under strict.
    s.scopes[0].vars.push(("api_token".into(), "leaked".into()));
    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
    let EnvSaveResult::Refused(msg) = &result else {
        panic!("expected Refused, got {result:?}");
    };
    assert!(
        msg.contains("api_token"),
        "refusal names the offending var: {msg}"
    );
    assert!(
        msg.contains("new"),
        "refusal signals it is newly authored: {msg}"
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
fn save_grandfathers_pre_existing_secret_with_warning() {
    // A secret literal already on disk is grandfathered: an unrelated edit saves
    // (with a warning naming it), rather than refusing the whole write.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"demo\"\n[vars]\napi_token = \"leaked\"\nbase_url = \"https://old\"\n",
    )
    .unwrap();
    let mut s = EnvEditorState {
        scopes: vec![EnvScope {
            kind: EnvScopeKind::Workspace,
            label: "Workspace".into(),
            vars: vec![
                ("api_token".into(), "leaked".into()), // pre-existing secret, untouched
                ("base_url".into(), "https://old".into()), // matches disk, edited below
            ],
        }],
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
    // Edit an unrelated var so the manifest is dirty and a write happens; the
    // secret var is left exactly as it sits on disk (grandfathered).
    s.scopes[0].vars[1].1 = "https://new".into();
    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
    let EnvSaveResult::Ok { warnings, .. } = &result else {
        panic!("expected Ok with warnings, got {result:?}");
    };
    assert!(
        warnings.iter().any(|w| w.contains("api_token")),
        "grandfathered secret is warned: {warnings:?}"
    );
    let text = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(text.contains("https://new"), "edit landed:\n{text}");
}

#[test]
fn save_warn_policy_never_blocks_new_secret() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let mut s = fixture();
    s.scopes[0].vars.push(("api_token".into(), "leaked".into()));
    let result = s.save(dir.path(), "demo", SecretPolicy::Warn);
    let EnvSaveResult::Ok { warnings, .. } = &result else {
        panic!("expected Ok under warn policy, got {result:?}");
    };
    assert!(
        warnings.iter().any(|w| w.contains("api_token")),
        "{warnings:?}"
    );
    let text = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        text.contains("api_token"),
        "warn policy wrote the value:\n{text}"
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
    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
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

    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
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
    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
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
    let result = s.save(dir.path(), "demo", SecretPolicy::Strict);
    assert!(matches!(result, EnvSaveResult::Ok { .. }));
    assert!(!s.is_dirty(), "save commits the active-profile change");
}

// --- Ephemeral peek + copy. ---

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
fn y_on_masked_row_without_reveal_is_a_no_op_hint() {
    // A masked/secret row keeps the never-expose-secrets gate: `y` refuses
    // until an explicit peek reveals it, hinting `p`.
    let mut s = fixture_with_session();
    on_session_row(&mut s);
    assert_eq!(s.handle_key(ch('y')), EnvKeyOutcome::Consumed);
    assert!(s.message.as_deref().unwrap().contains("nothing revealed"));
}

#[test]
fn y_on_visible_row_copies_value_directly() {
    // A plainly-visible (non-masked) value copies
    // outright with `y` — no peek needed. The app reads it back verbatim.
    let mut s = fixture(); // workspace base_url is not a secret
    s.focus = EnvFocus::VarRows;
    s.selected_row = 0;
    assert_eq!(s.handle_key(ch('y')), EnvKeyOutcome::CopyValue);
    assert_eq!(s.selected_row_value(), Some("https://api.example.com"));
    // No peek was involved and none is created.
    assert!(s.revealed_value().is_none());
}

#[test]
fn y_on_empty_visible_row_is_a_no_op_hint() {
    // Nothing to copy when the visible value is empty — a hint, not a silent
    // no-op (`y` must never quietly do nothing).
    let mut s = fixture();
    s.scopes[0].vars.push(("blank".into(), String::new()));
    s.focus = EnvFocus::VarRows;
    s.selected_row = s.scopes[0].vars.len() - 1;
    assert_eq!(s.handle_key(ch('y')), EnvKeyOutcome::Consumed);
    assert!(s.message.as_deref().unwrap().contains("empty"));
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
