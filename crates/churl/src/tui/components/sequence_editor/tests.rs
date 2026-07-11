use super::*;

fn seq() -> Sequence {
    Sequence {
        seq: 0,
        name: "Flow".to_owned(),
        on_error: OnError::Halt,
        steps: vec![
            SequenceStep {
                seq: 0,
                endpoint: "a/one.toml".to_owned(),
                extract: BTreeMap::new(),
                persist: Vec::new(),
            },
            SequenceStep {
                seq: 1,
                endpoint: "b/two.toml".to_owned(),
                extract: BTreeMap::new(),
                persist: Vec::new(),
            },
        ],
    }
}

fn editor() -> SequenceEditorState {
    SequenceEditorState::new(
        "Flow".to_owned(),
        PathBuf::from("sequences/flow.toml"),
        &seq(),
        vec!["a/one.toml".to_owned(), "c/three.toml".to_owned()],
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn typ(ed: &mut SequenceEditorState, text: &str) {
    for c in text.chars() {
        ed.handle_key(key(KeyCode::Char(c)));
    }
}

#[test]
fn starts_clean() {
    let ed = editor();
    assert!(!ed.is_dirty());
    assert_eq!(ed.steps.len(), 2);
}

#[test]
fn toggle_on_error_marks_dirty() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('o')));
    assert_eq!(ed.on_error, OnError::Continue);
    assert!(ed.is_dirty());
}

#[test]
fn delete_step_removes_it() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('d')));
    assert_eq!(ed.steps.len(), 1);
    assert!(ed.is_dirty());
}

#[test]
fn move_step_reorders() {
    let mut ed = editor();
    // Select second, move up.
    ed.handle_key(key(KeyCode::Char('j')));
    ed.handle_key(key(KeyCode::Char('K')));
    assert_eq!(ed.steps[0].endpoint, "b/two.toml");
    assert_eq!(ed.steps[1].endpoint, "a/one.toml");
    // to_sequence_checked renumbers by position.
    let out = ed.to_sequence_checked().unwrap();
    assert_eq!(out.steps[0].seq, 0);
    assert_eq!(out.steps[0].endpoint, "b/two.toml");
    assert_eq!(out.steps[1].seq, 1);
}

fn render_to_text(state: &SequenceEditorState) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let theme = Theme::default();
    terminal
        .draw(|frame| render(frame, frame.area(), state, &theme))
        .expect("draw");
    format!("{}", terminal.backend())
}

#[test]
fn rules_pane_shows_extraction_grammar_hint_when_focused() {
    // Adding a rule otherwise gives no guidance on the extract
    // syntax. The hint shows while the Rules pane is focused, and stays hidden
    // while on the Steps list.
    let mut ed = editor();
    // Steps-focused: no extraction hint yet.
    assert!(!render_to_text(&ed).contains("extract from response"));
    // Move focus to the Rules pane (l from a non-empty step list).
    ed.handle_key(key(KeyCode::Char('l')));
    assert_eq!(ed.focus, Focus::Rules);
    let out = render_to_text(&ed);
    assert!(out.contains("extract from response"));
    assert!(out.contains("status"));
    assert!(out.contains("header:<name>"));
    assert!(out.contains("$.json.path"));
}

#[test]
fn ctrl_j_k_also_reorder_steps() {
    // Ctrl-j / Ctrl-k are aliases for Shift-J / Shift-K reordering.
    // Ctrl must NOT fall through to the bare j/k selection nav.
    let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
    let mut ed = editor();
    // Select the second step, then Ctrl-k moves it up.
    ed.handle_key(key(KeyCode::Char('j')));
    ed.handle_key(ctrl(KeyCode::Char('k')));
    assert_eq!(ed.steps[0].endpoint, "b/two.toml");
    assert_eq!(ed.steps[1].endpoint, "a/one.toml");
    // Ctrl-j moves it back down.
    ed.handle_key(ctrl(KeyCode::Char('j')));
    assert_eq!(ed.steps[0].endpoint, "a/one.toml");
    assert_eq!(ed.steps[1].endpoint, "b/two.toml");
}

#[test]
fn add_step_via_picker() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('a'))); // open picker
    assert!(ed.picker.is_some());
    typ(&mut ed, "three"); // filter to c/three.toml
    ed.handle_key(key(KeyCode::Enter));
    assert!(ed.picker.is_none());
    assert_eq!(ed.steps.len(), 3);
    assert_eq!(ed.steps[2].endpoint, "c/three.toml");
}

#[test]
fn add_and_edit_extraction_rule() {
    let mut ed = editor();
    // Focus rules of step 0.
    ed.handle_key(key(KeyCode::Char('l')));
    assert_eq!(ed.focus, Focus::Rules);
    ed.handle_key(key(KeyCode::Char('a'))); // add rule → editing name
    typ(&mut ed, "token");
    ed.handle_key(key(KeyCode::Enter)); // commit name → chain to expr
    typ(&mut ed, "$.data.token");
    ed.handle_key(key(KeyCode::Enter)); // commit expr
    let out = ed.to_sequence_checked().unwrap();
    assert_eq!(out.steps[0].extract.get("token").unwrap(), "$.data.token");
}

#[test]
fn cancel_fresh_rule_drops_it() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('l')));
    ed.handle_key(key(KeyCode::Char('a'))); // add rule → editing name
    typ(&mut ed, "abc");
    ed.handle_key(key(KeyCode::Esc)); // cancel a fresh, unnamed rule
    assert_eq!(ed.steps[0].rules.len(), 0);
}

#[test]
fn dirty_guard_on_close() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('o'))); // dirty
    let outcome = ed.handle_key(key(KeyCode::Char('q')));
    assert_eq!(outcome, EditorOutcome::Consumed);
    assert!(ed.pending_close);
    // s → save+close
    assert_eq!(
        ed.handle_key(key(KeyCode::Char('s'))),
        EditorOutcome::SaveAndClose
    );
}

#[test]
fn clean_close_is_immediate() {
    let mut ed = editor();
    assert_eq!(ed.handle_key(key(KeyCode::Char('q'))), EditorOutcome::Close);
}

#[test]
fn save_round_trips_through_snapshot() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('o')));
    assert!(ed.is_dirty());
    ed.mark_saved();
    assert!(!ed.is_dirty());
}

#[test]
fn duplicate_rule_names_refuse_save() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('l'))); // focus rules of step 0
    // Add rule "x" = $.a
    ed.handle_key(key(KeyCode::Char('a')));
    typ(&mut ed, "x");
    ed.handle_key(key(KeyCode::Enter)); // name → chains to expr
    typ(&mut ed, "$.a");
    ed.handle_key(key(KeyCode::Enter));
    // Add a second rule also named "x".
    ed.handle_key(key(KeyCode::Char('a')));
    typ(&mut ed, "x");
    ed.handle_key(key(KeyCode::Enter));
    typ(&mut ed, "$.b");
    ed.handle_key(key(KeyCode::Enter));

    let err = ed.to_sequence_checked().unwrap_err();
    assert!(err.contains("duplicate rule name 'x'"), "{err}");
    assert!(err.contains("step 1"), "{err}");
}

#[test]
fn toggle_persist_flips_target_and_populates_persist_on_save() {
    // Note #6: `p` on a rule flips Run-only ⇄ Session; on save the rule name
    // lands in `SequenceStep.persist` (only when Session), never the value.
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('l'))); // focus rules of step 0
    ed.handle_key(key(KeyCode::Char('a'))); // add rule → editing name
    typ(&mut ed, "token");
    ed.handle_key(key(KeyCode::Enter)); // name → chains to expr
    typ(&mut ed, "$.data.token");
    ed.handle_key(key(KeyCode::Enter));
    // Default is Run-only: not in persist.
    let out = ed.to_sequence_checked().unwrap();
    assert!(out.steps[0].persist.is_empty(), "default is Run-only");
    // Flip to Session.
    ed.handle_key(key(KeyCode::Char('p')));
    let out = ed.to_sequence_checked().unwrap();
    assert_eq!(out.steps[0].persist, vec!["token".to_owned()]);
    // The captured value is NOT in persist — only the rule name.
    assert!(!out.steps[0].persist.iter().any(|n| n.contains("$.")));
    // Flip back to Run-only.
    ed.handle_key(key(KeyCode::Char('p')));
    assert!(
        ed.to_sequence_checked().unwrap().steps[0]
            .persist
            .is_empty()
    );
}

#[test]
fn persist_seeds_from_loaded_step_and_shows_session_marker() {
    // Loading a step whose `persist` lists a rule shows the ` →session` marker
    // and round-trips the flag back out on save.
    let sequence = Sequence {
        seq: 0,
        name: "Flow".to_owned(),
        on_error: OnError::Halt,
        steps: vec![SequenceStep {
            seq: 0,
            endpoint: "auth/login.toml".to_owned(),
            extract: BTreeMap::from([("token".to_owned(), "$.token".to_owned())]),
            persist: vec!["token".to_owned()],
        }],
    };
    let mut ed = SequenceEditorState::new(
        "Flow".to_owned(),
        PathBuf::from("sequences/flow.toml"),
        &sequence,
        vec!["auth/login.toml".to_owned()],
    );
    assert_eq!(ed.steps[0].persist, vec![true]);
    // Focus the rules pane so the row renders, then assert the marker shows.
    ed.handle_key(key(KeyCode::Char('l')));
    let out = render_to_text(&ed);
    assert!(out.contains("→session"), "session marker shown:\n{out}");
    // Round-trips back out unchanged.
    assert_eq!(
        ed.to_sequence_checked().unwrap().steps[0].persist,
        vec!["token".to_owned()]
    );
}

#[test]
fn session_marker_style_stays_legible_on_selected_row() {
    // On the highlighted row the marker sits on the
    // `selection` fill. The real bug was fg==bg — the
    // marker's own hue tracks the selection background and vanished. Assert
    // the property that actually means "legible": foreground != background,
    // for BOTH shipped themes — plus that it adapts vs the plain row and
    // stays subordinate (DIM).
    for theme in [Theme::dark(), Theme::light()] {
        let plain = session_marker_style(&theme, false);
        let highlighted = session_marker_style(&theme, true);
        assert_ne!(
            plain, highlighted,
            "the marker must adapt when its row is selected"
        );
        // The one thing "legible" actually means: the glyph is not the same
        // colour as the fill it sits on.
        assert_ne!(
            highlighted.fg, highlighted.bg,
            "the highlighted marker must not be fg==bg (invisible) on the selection fill"
        );
        assert_eq!(
            highlighted.bg, theme.selection.bg,
            "the highlighted marker carries the selection background"
        );
        assert!(
            highlighted
                .add_modifier
                .contains(ratatui::style::Modifier::DIM),
            "the highlighted marker stays visually subordinate (DIM)"
        );
    }
}

#[test]
fn empty_named_rule_reconciles_after_save() {
    let mut ed = editor();
    ed.handle_key(key(KeyCode::Char('l'))); // focus rules
    ed.handle_key(key(KeyCode::Char('a'))); // add rule → editing name (fresh)
    // Commit an EMPTY name (Enter, not Esc) — keeps a ("", "") rule; then leave
    // the chained expr edit untouched.
    ed.handle_key(key(KeyCode::Enter));
    ed.handle_key(key(KeyCode::Esc));
    assert_eq!(ed.steps[0].rules.len(), 1, "empty-named rule is present");
    assert!(ed.is_dirty(), "adding a rule dirties the working copy");
    // to_sequence_checked drops the empty-named rule from output...
    assert!(
        ed.to_sequence_checked().unwrap().steps[0]
            .extract
            .is_empty()
    );
    // ...and mark_saved purges it so dirty reconciles (no working≠disk gap).
    ed.mark_saved();
    assert_eq!(ed.steps[0].rules.len(), 0, "empty rule purged on save");
    assert!(!ed.is_dirty(), "no dirty-after-save discrepancy");
}
