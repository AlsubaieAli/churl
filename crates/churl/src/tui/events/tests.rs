use super::*;

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn default_map_lookups() {
    let keymap = KeyMap::default();
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(Action::Quit)
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Action::Quit)
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Tab, KeyModifiers::NONE)),
        Some(Action::FocusNext)
    );
    // crossterm reports Shift-Tab as BackTab with the SHIFT modifier set.
    assert_eq!(
        keymap.lookup(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Some(Action::FocusPrev)
    );
    // Uppercase G arrives as Char('G') + SHIFT.
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
        Some(Action::Bottom)
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('/'), KeyModifiers::NONE)),
        Some(Action::OpenSearch)
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
        None
    );
}

/// `}` / `{` are GLOBAL buffer next/prev — they must resolve from
/// any pane (base map, not a pane overlay). Verified from a non-Request pane
/// context so a Request-overlay binding can never be what makes them work.
#[test]
fn brace_keys_are_global_buffer_nav() {
    let keymap = KeyMap::default();
    // Global lookup (no pane context).
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('}'), KeyModifiers::NONE)),
        Some(Action::BufferNext),
        "}} → next buffer globally"
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('{'), KeyModifiers::NONE)),
        Some(Action::BufferPrev),
        "{{ → prev buffer globally"
    );
    // Fires from a NON-Request pane too (Response overlay has no `{`/`}`), so
    // it is the global map — not a pane overlay — that resolves them.
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('}'), KeyModifiers::NONE),
            PaneCtx::Response
        ),
        Some(Action::BufferNext),
        "}} works from the Response pane (global)"
    );
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('{'), KeyModifiers::NONE),
            PaneCtx::Explorer
        ),
        Some(Action::BufferPrev),
        "{{ works from the Explorer pane (global)"
    );
    // `[` / `]` remain Request-pane sub-tab nav — untouched, and NOT global.
    assert_eq!(
        keymap.lookup(press(KeyCode::Char(']'), KeyModifiers::NONE)),
        None,
        "] stays request-overlay only, not global"
    );
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char(']'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::TabNext),
        "] is still request sub-tab next"
    );
}

#[test]
fn config_override_wins() {
    let overrides = BTreeMap::from([
        ("q".to_string(), "focus-response".to_string()),
        ("ctrl-p".to_string(), "open-palette".to_string()),
    ]);
    let keymap = KeyMap::with_overrides(&overrides).unwrap();
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(Action::FocusResponse)
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        Some(Action::OpenPalette)
    );
    // Untouched defaults survive.
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('j'), KeyModifiers::NONE)),
        Some(Action::Down)
    );
}

#[test]
fn bad_action_name_errors() {
    let overrides = BTreeMap::from([("q".to_string(), "explode".to_string())]);
    let err = KeyMap::with_overrides(&overrides).unwrap_err();
    assert!(err.to_string().contains("explode"), "{err}");
}

#[test]
fn bad_key_combination_errors() {
    let overrides = BTreeMap::from([("ctrl-".to_string(), "quit".to_string())]);
    assert!(KeyMap::with_overrides(&overrides).is_err());
}

#[test]
fn action_name_round_trip() {
    for action in Action::all() {
        assert_eq!(action.name().parse::<Action>().unwrap(), action);
    }
}

#[test]
fn iter_covers_every_default_binding() {
    let keymap = KeyMap::default();
    let bindings: Vec<_> = keymap.iter().collect();
    // Every binding round-trips through lookup.
    for (combo, action) in &bindings {
        assert_eq!(keymap.lookup((*combo).into()), Some(*action));
    }
    // `f` is bound to Jump by default.
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('f'), KeyModifiers::NONE)),
        Some(Action::Jump)
    );
    assert!(bindings.iter().any(|(_, a)| *a == Action::Jump));
}

#[test]
fn combos_for_returns_sorted_bindings() {
    let keymap = KeyMap::default();
    // Quit is bound to both `q` and `ctrl-c`.
    let combos = keymap.combos_for(Action::Quit);
    assert_eq!(combos.len(), 2);
    let mut sorted = combos.clone();
    sorted.sort();
    assert_eq!(combos, sorted, "combos_for must be sorted");
    // SwitchProfile has no default binding.
    assert!(keymap.combos_for(Action::SwitchProfile).is_empty());
}

#[test]
fn overlay_lookup_precedence() {
    let keymap = KeyMap::default();
    // `1` has no global binding (there are no pane-focus digits); inside the
    // Request overlay it is Tab1.
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('1'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('1'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::Tab1)
    );
    // A key not in the overlay falls through to the global map.
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('j'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::Down)
    );
    // `d` in the Explorer overlay is Delete; in Request it is RowDelete.
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('d'), KeyModifiers::NONE),
            PaneCtx::Explorer
        ),
        Some(Action::Delete)
    );
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('d'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::RowDelete)
    );
    // The UrlBar overlay: `m` cycles method, `i` edits URL.
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('m'), KeyModifiers::NONE),
            PaneCtx::UrlBar
        ),
        Some(Action::MethodCycle)
    );
}

/// Arrow keys navigate the endpoint/sequence explorer, mirroring the global
/// `k`/`j`/`h`/`l` (owner drive-test 2026-07-10). Scoped to the Explorer
/// overlay so Left/Right→Collapse/Expand never leak into other panes.
#[test]
fn explorer_overlay_arrow_keys_navigate() {
    let keymap = KeyMap::default();
    assert_eq!(
        keymap.lookup_ctx(press(KeyCode::Up, KeyModifiers::NONE), PaneCtx::Explorer),
        Some(Action::Up)
    );
    assert_eq!(
        keymap.lookup_ctx(press(KeyCode::Down, KeyModifiers::NONE), PaneCtx::Explorer),
        Some(Action::Down)
    );
    assert_eq!(
        keymap.lookup_ctx(press(KeyCode::Left, KeyModifiers::NONE), PaneCtx::Explorer),
        Some(Action::Collapse)
    );
    assert_eq!(
        keymap.lookup_ctx(press(KeyCode::Right, KeyModifiers::NONE), PaneCtx::Explorer),
        Some(Action::Expand)
    );
    // Left/Right stay Explorer-scoped: no global bind, so Collapse/Expand
    // never leak into other panes via the arrows.
    assert_eq!(
        keymap.lookup(press(KeyCode::Left, KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Right, KeyModifiers::NONE)),
        None
    );
}

#[test]
fn overlay_override_parses_and_wins() {
    let overlays = BTreeMap::from([(
        "request".to_string(),
        BTreeMap::from([("x".to_string(), "tab-next".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('x'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::TabNext)
    );
    // Default overlay bindings survive.
    assert_eq!(
        keymap.lookup_ctx(
            press(KeyCode::Char('1'), KeyModifiers::NONE),
            PaneCtx::Request
        ),
        Some(Action::Tab1)
    );
}

#[test]
fn unknown_overlay_table_errors() {
    let overlays = BTreeMap::from([(
        "bogus".to_string(),
        BTreeMap::from([("x".to_string(), "tab-next".to_string())]),
    )]);
    let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
    assert!(err.to_string().contains("bogus"), "{err}");
}

#[test]
fn bad_overlay_action_errors() {
    let overlays = BTreeMap::from([(
        "request".to_string(),
        BTreeMap::from([("x".to_string(), "explode".to_string())]),
    )]);
    let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
    assert!(err.to_string().contains("explode"), "{err}");
}

#[test]
fn overlay_combos_and_iter_cover_bindings() {
    let keymap = KeyMap::default();
    assert_eq!(
        keymap.overlay_combos_for(PaneCtx::Request, Action::Tab1),
        vec!["1"]
    );
    let request: Vec<_> = keymap.iter_overlay(PaneCtx::Request).collect();
    assert!(request.iter().any(|(_, a)| *a == Action::TabNext));
    // Save is a global binding, not an overlay one.
    assert_eq!(keymap.combos_for(Action::Save), vec!["w"]);
}

#[test]
fn digit_binds_only_act_in_request() {
    let keymap = KeyMap::default();
    // No global digit binds (1–4).
    for d in ['1', '2', '3', '4'] {
        assert_eq!(
            keymap.lookup(press(KeyCode::Char(d), KeyModifiers::NONE)),
            None,
            "'{d}' must have no global binding"
        );
    }
    // 1–4 still act as tab jumps inside the Request overlay.
    for (d, action) in [
        ('1', Action::Tab1),
        ('2', Action::Tab2),
        ('3', Action::Tab3),
        ('4', Action::Tab4),
    ] {
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char(d), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(action)
        );
        // And no other pane overlay binds them.
        for ctx in [PaneCtx::Explorer, PaneCtx::UrlBar, PaneCtx::Response] {
            assert_eq!(
                keymap.lookup_ctx(press(KeyCode::Char(d), KeyModifiers::NONE), ctx),
                None,
                "'{d}' must be inert in {ctx:?}"
            );
        }
    }
}

#[test]
fn default_leader_map() {
    let keymap = KeyMap::default();
    assert_eq!(keymap.leader(), key!(space).normalized());
    assert!(keymap.is_leader(press(KeyCode::Char(' '), KeyModifiers::NONE)));
    // Direct root actions.
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('e'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::ToggleExplorer))
    );
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('c'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::Cancel))
    );
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::Quit))
    );
    // `<leader>r` reloads the workspace from disk (free at root; plain `r` is
    // the Explorer-overlay rename, a distinct non-leader state).
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::Reload))
    );
    // Submenu descents.
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('s'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("sequences".to_owned()))
    );
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('l'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("load".to_owned()))
    );
    // `<leader><leader>` (Space) is the endpoint/request picker — moved off
    // `f` (owner drive-test 2026-07-10). A leader continuation of Space is
    // fine; `f` is now free at root.
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char(' '), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::QuickJumpRequests))
    );
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('f'), KeyModifiers::NONE)),
        None
    );
    // An unbound root continuation returns None (the popup dismisses).
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
        None
    );
}

#[test]
fn leader_submenu_lookups() {
    let keymap = KeyMap::default();
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('r'), KeyModifiers::NONE)),
        // `<leader>s r` routes to the run-flavored chooser.
        Some(Action::RunSequencePick)
    );
    assert_eq!(
        // `<leader>s n` creates a new sequence (replaced the buggy `<leader>s a`).
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('n'), KeyModifiers::NONE)),
        Some(Action::NewSequence)
    );
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('a'), KeyModifiers::NONE)),
        None
    );
    // `<leader>s <leader>` (Space) is the single sequence finder (mirrors
    // `<leader><leader>` for endpoints); the former `o`/`f` binds are gone
    // (owner drive-test 2026-07-10).
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char(' '), KeyModifiers::NONE)),
        Some(Action::OpenSequencePicker)
    );
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('o'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('f'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        keymap.leader_sub_lookup("load", press(KeyCode::Char('c'), KeyModifiers::NONE)),
        Some(Action::OpenLoadRunner)
    );
    // `<leader>l <leader>` (Space) picks an endpoint first (was `f`).
    assert_eq!(
        keymap.leader_sub_lookup("load", press(KeyCode::Char(' '), KeyModifiers::NONE)),
        Some(Action::OpenLoadRunnerPick)
    );
    assert_eq!(
        keymap.leader_sub_lookup("load", press(KeyCode::Char('f'), KeyModifiers::NONE)),
        None
    );
    // `<leader>l s` is reserved (composable-runs) — must stay unbound.
    assert_eq!(
        keymap.leader_sub_lookup("load", press(KeyCode::Char('s'), KeyModifiers::NONE)),
        None
    );
    // An unknown submenu name yields no action.
    assert_eq!(
        keymap.leader_sub_lookup("nope", press(KeyCode::Char('a'), KeyModifiers::NONE)),
        None
    );
}

#[test]
fn send_reclaimed_from_leader() {
    let keymap = KeyMap::default();
    // `<leader>s` descends into the sequences submenu — it is NOT Send.
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('s'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("sequences".to_owned()))
    );
    // Send is nowhere in the leader tree; Ctrl-S still sends.
    assert!(
        !keymap.iter_leader().any(|(_, a)| a == Action::Send),
        "Send must not appear in any leader map"
    );
    assert_eq!(
        keymap.lookup(press(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        Some(Action::Send)
    );
}

#[test]
fn flat_sequence_actions_not_root_bound() {
    let keymap = KeyMap::default();
    // RunSequence / EditSequence are reachable only through the sequences
    // submenu now — not as direct root binds.
    for entry in keymap.leader_root.values() {
        if let LeaderEntry::Act(a) = entry {
            assert!(
                !matches!(a, Action::RunSequence | Action::EditSequence),
                "{a:?} must not be a direct root leader bind"
            );
        }
    }
    // leader_combos_for reports the full chord path for submenu actions.
    // `<leader>s r` maps to the run-flavored chooser.
    assert_eq!(
        keymap.leader_combos_for(Action::RunSequencePick),
        vec!["s r"]
    );
    assert_eq!(
        keymap.leader_combos_for(Action::OpenLoadRunner),
        vec!["l c"]
    );
}

#[test]
fn leader_table_parses_and_overrides() {
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("x".to_string(), "save".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::Save))
    );
    // Default root continuations survive.
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(LeaderEntry::Act(Action::Quit))
    );
    assert_eq!(keymap.leader_combos_for(Action::Save), vec!["x"]);
}

#[test]
fn leader_submenu_table_parses() {
    // `[keys.leader.sequences]` binds a submenu action.
    let overlays = BTreeMap::from([(
        "leader.sequences".to_string(),
        BTreeMap::from([("z".to_string(), "run-sequence".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('z'), KeyModifiers::NONE)),
        Some(Action::RunSequence)
    );
    // Defaults in the submenu survive.
    assert_eq!(
        keymap.leader_sub_lookup("sequences", press(KeyCode::Char('n'), KeyModifiers::NONE)),
        Some(Action::NewSequence)
    );
}

#[test]
fn leader_descent_token_parses() {
    // `[keys.leader]` accepts a `"+<submenu>"` descent token.
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("g".to_string(), "+load".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('g'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("load".to_owned()))
    );
}

#[test]
fn bad_leader_table_action_errors() {
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("x".to_string(), "explode".to_string())]),
    )]);
    let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
    assert!(err.to_string().contains("explode"), "{err}");
}

#[test]
fn leader_descent_to_new_submenu_parses() {
    // Any non-empty `"+name"` descent is valid — submenus are dynamic.
    // A dangling one (no matching table) parses fine and is caught later by
    // the load-time validator, not the parser.
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("x".to_string(), "+bogus".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("bogus".to_owned()))
    );
}

#[test]
fn empty_leader_descent_token_errors() {
    // A bare `"+"` (no submenu name) is still a hard error.
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("x".to_string(), "+".to_string())]),
    )]);
    let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
    assert!(err.to_string().contains("empty leader descent"), "{err}");
}

#[test]
fn leader_submenu_table_creates_new_submenu() {
    // `[keys.leader.git]` creates a brand-new submenu (dynamic).
    let overlays = BTreeMap::from([
        (
            "leader".to_string(),
            BTreeMap::from([("g".to_string(), "+git".to_string())]),
        ),
        (
            "leader.git".to_string(),
            BTreeMap::from([("s".to_string(), "save".to_string())]),
        ),
    ]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    assert_eq!(
        keymap.leader_root_lookup(press(KeyCode::Char('g'), KeyModifiers::NONE)),
        Some(LeaderEntry::Submenu("git".to_owned()))
    );
    assert_eq!(
        keymap.leader_sub_lookup("git", press(KeyCode::Char('s'), KeyModifiers::NONE)),
        Some(Action::Save)
    );
    // The new submenu appears in the which-key submenu list, reachable via `g`.
    assert!(
        keymap
            .leader_menu_combos()
            .iter()
            .any(|(k, l)| k == "g" && l == "git")
    );
    // And a clean end-to-end config produces no validator warnings.
    let overlays_config = overlays.clone();
    assert!(
        keymap
            .validate(&BTreeMap::new(), &overlays_config)
            .is_empty()
    );
}

#[test]
fn bad_leader_submenu_table_action_errors() {
    // The submenu name is now free, but a bad action inside it still fails.
    let overlays = BTreeMap::from([(
        "leader.git".to_string(),
        BTreeMap::from([("x".to_string(), "explode".to_string())]),
    )]);
    let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
    assert!(err.to_string().contains("explode"), "{err}");
}

#[test]
fn set_leader_remaps() {
    let mut keymap = KeyMap::default();
    keymap.set_leader("ctrl-b").unwrap();
    assert!(keymap.is_leader(press(KeyCode::Char('b'), KeyModifiers::CONTROL)));
    assert!(!keymap.is_leader(press(KeyCode::Char(' '), KeyModifiers::NONE)));
    assert!(keymap.set_leader("ctrl-").is_err());
}

#[test]
fn validate_clean_default_config_has_no_warnings() {
    // The out-of-the-box keymap must produce ZERO warnings — documented
    // single-pane shadows (Response `h`/`/`, Request `1`–`4`) are lawful,
    // and so is Space bound as a leader *continuation*
    // (`<leader><leader>`/`<leader>s <leader>`/`<leader>l <leader>`, owner
    // drive-test 2026-07-10): `validate` only flags the leader key when it
    // is (re)bound in the GLOBAL map or a PANE overlay, never as a leader
    // continuation.
    let keymap = KeyMap::default();
    assert!(
        keymap
            .validate(&BTreeMap::new(), &BTreeMap::new())
            .is_empty(),
        "default keymap warnings: {:?}",
        keymap.validate(&BTreeMap::new(), &BTreeMap::new())
    );
}

#[test]
fn validate_flags_leader_key_bound_as_action() {
    // The dead `space=…` case: the leader key also bound as a global action.
    let global = BTreeMap::from([("space".to_string(), "save".to_string())]);
    let keymap = KeyMap::with_all_overrides(&global, &BTreeMap::new()).unwrap();
    let warnings = keymap.validate(&global, &BTreeMap::new());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("leader key"), "{warnings:?}");
}

#[test]
fn validate_flags_dangling_descent() {
    // A `"+name"` descent with no matching `[keys.leader.name]` table.
    let overlays = BTreeMap::from([(
        "leader".to_string(),
        BTreeMap::from([("g".to_string(), "+git".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    let warnings = keymap.validate(&BTreeMap::new(), &overlays);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("git"), "{warnings:?}");
    assert!(
        warnings[0].contains("no `[keys.leader.git]`"),
        "{warnings:?}"
    );
}

#[test]
fn validate_flags_orphan_submenu() {
    // A `[keys.leader.name]` table that no descent points to.
    let overlays = BTreeMap::from([(
        "leader.git".to_string(),
        BTreeMap::from([("s".to_string(), "save".to_string())]),
    )]);
    let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
    let warnings = keymap.validate(&BTreeMap::new(), &overlays);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("unreachable"), "{warnings:?}");
}

#[test]
fn validate_flags_duplicate_combo_in_scope() {
    // Two combo strings normalizing to the same key within one scope
    // (modifier order differs, but they resolve to the same combination).
    let global = BTreeMap::from([
        ("ctrl-shift-a".to_string(), "quit".to_string()),
        ("shift-ctrl-a".to_string(), "save".to_string()),
    ]);
    let keymap = KeyMap::with_all_overrides(&global, &BTreeMap::new()).unwrap();
    let warnings = keymap.validate(&global, &BTreeMap::new());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("silently wins"), "{warnings:?}");
}

#[test]
fn validate_flags_global_shadowed_in_every_overlay() {
    // A global bind rebound in all four pane overlays — globally unreachable.
    let global = BTreeMap::from([("ctrl-g".to_string(), "quit".to_string())]);
    let overlays = BTreeMap::from([
        (
            "explorer".to_string(),
            BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
        ),
        (
            "urlbar".to_string(),
            BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
        ),
        (
            "request".to_string(),
            BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
        ),
        (
            "response".to_string(),
            BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
        ),
    ]);
    let keymap = KeyMap::with_all_overrides(&global, &overlays).unwrap();
    let warnings = keymap.validate(&global, &overlays);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("globally unreachable"), "{warnings:?}");
}

#[test]
fn fuzzy_empty_query_returns_all() {
    let items = vec!["alpha".to_string(), "beta".to_string()];
    let mut finder = FuzzyFinder::new();
    assert_eq!(finder.filter("", &items), vec![0, 1]);
}

#[test]
fn fuzzy_match_ordering() {
    let items = vec![
        "users/list all users".to_string(),
        "orders/create order".to_string(),
        "users/get user".to_string(),
    ];
    let mut finder = FuzzyFinder::new();
    let hits = finder.filter("user", &items);
    // Both user endpoints match; the order collection does not.
    assert!(!hits.contains(&1));
    assert_eq!(hits.len(), 2);
    // Case-insensitive.
    let hits = finder.filter("USER", &items);
    assert_eq!(hits.len(), 2);
}
