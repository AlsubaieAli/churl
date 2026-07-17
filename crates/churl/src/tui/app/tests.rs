use super::*;
use churl_core::model::{OnError, Timing};

#[test]
fn export_target_stays_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A relative path resolves under the (canonicalized) root.
    let target = export_target(root, "exports/api.json").unwrap();
    let canon_root = root.canonicalize().unwrap();
    assert!(target.starts_with(&canon_root), "{target:?}");
    assert!(target.ends_with("exports/api.json"));
    // A nested `..` that stays inside is fine.
    let ok = export_target(root, "exports/../api.json").unwrap();
    assert_eq!(ok, canon_root.join("api.json"));
}

#[test]
fn export_target_rejects_escaping_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(export_target(root, "../escape.json").is_err());
    assert!(export_target(root, "exports/../../escape.json").is_err());
    assert!(export_target(root, "/etc/passwd").is_err());
    assert!(export_target(root, "   ").is_err());
}

#[cfg(unix)]
#[test]
fn export_target_rejects_symlinked_component() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();
    // A symlinked dir inside the root that points elsewhere: lexically the
    // target is "under" the root, but the write would follow the link out.
    std::os::unix::fs::symlink(outside.path(), root.join("exports")).unwrap();
    assert!(
        export_target(root, "exports/leak.json").is_err(),
        "a symlinked component must not tunnel out of the workspace"
    );
}

#[test]
fn default_export_path_slugifies() {
    assert_eq!(default_export_path("My API"), "exports/my-api.json");
    assert_eq!(default_export_path("  "), "exports/export.json");
}

fn meta() -> ResponseMeta {
    ResponseMeta {
        method: "GET".to_owned(),
        url: "https://api.test/x".to_owned(),
        endpoint_path: Some("users/get.toml".to_owned()),
        executed_at_ms: 1,
    }
}

fn response() -> Response {
    Response {
        status: 200,
        headers: Vec::new(),
        body: b"{}".to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(3),
        },
    }
}

/// A minimal loaded endpoint buffer (no workspace) so white-box tests can set
/// per-buffer state (`in_flight`, `response`, editor) that lives per-buffer,
/// not as flat `App` fields.
fn open_bare_endpoint(app: &mut App) {
    let endpoint = Endpoint {
        seq: 0,
        name: "test".to_owned(),
        assertions: Vec::new(),
        request: Request {
            method: churl_core::model::Method::Get,
            url: "https://api.test/x".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        },
    };
    app.open_or_focus_buffer(SelectedEndpoint {
        display_path: "test".to_owned(),
        file: std::path::PathBuf::from("/tmp/test.toml"),
        collection: 0,
        endpoint,
    });
}

/// White-box: put a fabricated in-flight request on the active buffer.
fn set_active_in_flight(app: &mut App, generation: u64) {
    let handle = tokio::spawn(async {});
    app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: handle.abort_handle(),
        generation,
        meta: meta(),
    });
}

/// White-box: whether the active buffer has an in-flight request.
fn active_in_flight_is_some(app: &App) -> bool {
    app.active_endpoint_buffer()
        .is_some_and(|b| b.in_flight.is_some())
}

/// A response whose generation no longer matches the in-flight request (after
/// a cancel+resend) must be dropped without touching the pane.
#[tokio::test]
async fn stale_generation_response_is_dropped() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    open_bare_endpoint(&mut app);
    app.generation = 5;
    set_active_in_flight(&mut app, 5);

    // A late result from an older generation is ignored…
    app.on_response(4, Ok(response()), meta());
    assert!(matches!(app.response(), ResponseState::Idle));
    assert!(
        active_in_flight_is_some(&app),
        "in-flight preserved on stale drop"
    );

    // …the matching generation lands and clears the in-flight slot.
    app.on_response(5, Ok(response()), meta());
    assert!(matches!(app.response(), ResponseState::Done { .. }));
    assert!(!active_in_flight_is_some(&app));
}

/// Puts a fresh app in insert mode on the request pane's Body tab (where the
/// edtui editor lives). A bare endpoint buffer is loaded so the per-buffer
/// editor/tabs exist (they moved off `App` into the active buffer).
fn insert_mode_app(keymap: KeyMap) -> App {
    let mut app = App::new(None, keymap).unwrap();
    open_bare_endpoint(&mut app);
    app.focus = Pane::Request;
    app.test_tabs().active = RequestTab::Body;
    app.test_editor().mode = EditorMode::Insert;
    app
}

/// Insert mode + Ctrl-S dispatches Send instead of reaching edtui. Proof of
/// interception is an OBSERVABLE Send effect that only Send produces: with a
/// request already in flight, Send's in-flight guard emits the distinctive
/// "already in flight" message. If Ctrl-S instead reached edtui (interception
/// removed), edtui would ignore the Ctrl-modified char and leave `message`
/// None — so this fails when the insert-mode `control_intercept` block is
/// removed. The editor is also untouched and insert mode is preserved.
#[tokio::test]
async fn insert_mode_ctrl_s_dispatches_send() {
    let mut app = insert_mode_app(KeyMap::default());
    // A request already in flight: Send's guard produces an observable message.
    set_active_in_flight(&mut app, 1);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("request already in flight — ctrl-c to cancel"),
        "Ctrl-S must dispatch Send (observable via its in-flight guard)"
    );
    assert_eq!(String::from(app.test_editor().lines.clone()), "");
    assert_eq!(app.test_editor().mode, EditorMode::Insert);
    assert!(!app.should_quit);
}

/// Insert mode + Ctrl-C with a request in flight cancels it (never quits,
/// never reaches edtui).
#[tokio::test]
async fn insert_mode_ctrl_c_cancels_in_flight() {
    let mut app = insert_mode_app(KeyMap::default());
    app.generation = 1;
    set_active_in_flight(&mut app, 1);
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(!active_in_flight_is_some(&app));
    assert!(matches!(app.response(), ResponseState::Cancelled));
    assert!(!app.should_quit);
    assert_eq!(String::from(app.test_editor().lines.clone()), "");
}

/// Insert mode + Ctrl-C with nothing in flight falls back to Quit
/// (the Ctrl-C-in-request-context behaviour, unchanged from Normal mode).
#[test]
fn insert_mode_ctrl_c_without_in_flight_quits() {
    let mut app = insert_mode_app(KeyMap::default());
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(app.should_quit);
}

/// Plain `s`/`c` in insert mode are text input and still reach edtui.
#[test]
fn insert_mode_plain_chars_reach_edtui() {
    let mut app = insert_mode_app(KeyMap::default());
    for c in ['s', 'c'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    assert_eq!(String::from(app.test_editor().lines.clone()), "sc");
    assert!(!app.should_quit);
    assert!(app.message.is_none());
}

/// The interception resolves through the keymap: a remapped send key with
/// CONTROL is intercepted in insert mode too. Discriminated by the same
/// observable Send effect (the in-flight guard's message), so it fails if the
/// insert-mode `control_intercept` block is removed.
#[tokio::test]
async fn insert_mode_remapped_ctrl_send_is_intercepted() {
    let overrides = std::collections::BTreeMap::from([("ctrl-b".to_string(), "send".to_string())]);
    let mut app = insert_mode_app(KeyMap::with_overrides(&overrides).unwrap());
    set_active_in_flight(&mut app, 1);
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("request already in flight — ctrl-c to cancel"),
        "remapped Ctrl-B must dispatch Send (observable via its in-flight guard)"
    );
    assert_eq!(String::from(app.test_editor().lines.clone()), "");
}

/// Builds a minimal workspace with one collection + endpoint and two profiles.
fn workspace_fixture(root: &Path) -> App {
    std::fs::write(
            root.join("churl.toml"),
            "name = \"demo\"\n\n[vars]\nbase = \"ws\"\n\n[[profiles]]\nname = \"dev\"\n[profiles.vars]\nhost = \"dev.test\"\n\n[[profiles]]\nname = \"prod\"\n[profiles.vars]\nhost = \"prod.test\"\n",
        )
        .unwrap();
    let coll = root.join("users");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
            coll.join("get.toml"),
            "seq = 0\nname = \"Get user\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{host}}/users\"\n",
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    App::new(ws, KeyMap::default()).unwrap()
}

// ---- sequence runner state machine (injected outcomes, no server) ----

/// Builds a workspace with three GET endpoints and a `sequences/flow.toml`
/// running them in order, then opens the runner (client `None`, so steps stay
/// in the `Running` stub until an outcome is injected).
fn sequence_app(root: &Path, on_error: &str, extract: &str) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    for name in ["one", "two", "three"] {
        std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
    }
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"{on_error}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n{extract}\n[[step]]\nseq = 1\nendpoint = \"api/two.toml\"\n\n[[step]]\nseq = 2\nendpoint = \"api/three.toml\"\n"
            ),
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let file = seq_dir.join("flow.toml");
    let sequence = churl_core::persistence::load_sequence(&file).unwrap();
    app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
        name: sequence.name.clone(),
        file,
        sequence,
    });
    app
}

fn ok_resp(status: u16, body: &str) -> Response {
    Response {
        status,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: std::time::Duration::from_millis(5),
        },
    }
}

fn runner_gen(app: &App) -> u64 {
    app.sequence_runner().unwrap().run_generation
}

fn step_status(app: &App, i: usize) -> StepStatus {
    app.sequence_runner().unwrap().steps[i].status.clone()
}

#[test]
fn sequence_runner_opens_with_first_step_running() {
    let dir = tempfile::tempdir().unwrap();
    let app = sequence_app(dir.path(), "halt", "");
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Run);
    let runner = app.sequence_runner().unwrap();
    assert_eq!(runner.steps.len(), 3);
    assert_eq!(runner.current, Some(0));
    assert_eq!(step_status(&app, 0), StepStatus::Running);
    assert_eq!(step_status(&app, 1), StepStatus::Pending);
}

#[test]
fn sequence_run_halts_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, "{}")));
    assert_eq!(step_status(&app, 0), StepStatus::Ok(200));
    assert_eq!(step_status(&app, 1), StepStatus::Running);
    // Step 1 fails with 500 → halt → step 2 skipped, run finished.
    app.on_sequence_step(run_gen, 1, Ok(ok_resp(500, "err")));
    assert_eq!(step_status(&app, 1), StepStatus::Failed(500));
    assert_eq!(step_status(&app, 2), StepStatus::Skipped);
    assert!(app.sequence_runner().unwrap().finished);
}

#[test]
fn sequence_run_continue_runs_all() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "continue", "");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Ok(ok_resp(500, "err")));
    assert_eq!(step_status(&app, 0), StepStatus::Failed(500));
    // Continue: step 1 runs despite the earlier failure.
    assert_eq!(step_status(&app, 1), StepStatus::Running);
    app.on_sequence_step(run_gen, 1, Ok(ok_resp(200, "{}")));
    assert_eq!(step_status(&app, 2), StepStatus::Running);
    app.on_sequence_step(run_gen, 2, Ok(ok_resp(200, "{}")));
    assert!(app.sequence_runner().unwrap().finished);
}

#[test]
fn sequence_run_transport_error_halts() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Err("connection refused".to_owned()));
    assert!(matches!(step_status(&app, 0), StepStatus::HttpError(_)));
    assert_eq!(step_status(&app, 1), StepStatus::Skipped);
    assert_eq!(step_status(&app, 2), StepStatus::Skipped);
}

#[test]
fn sequence_run_accumulates_and_chains_extracted() {
    let dir = tempfile::tempdir().unwrap();
    // Step one extracts token = $.token.
    let mut app = sequence_app(dir.path(), "halt", "[step.extract]\ntoken = \"$.token\"");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, r#"{"token":"XYZ"}"#)));
    let runner = app.sequence_runner().unwrap();
    assert_eq!(
        runner.extracted.get("token").map(String::as_str),
        Some("XYZ")
    );
    assert_eq!(runner.steps[0].extracted.get("token").unwrap(), "XYZ");
    assert_eq!(step_status(&app, 1), StepStatus::Running);
}

#[test]
fn sequence_extract_error_halts() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "[step.extract]\nx = \"$.missing\"");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, "{}")));
    assert!(matches!(step_status(&app, 0), StepStatus::ExtractError(_)));
    assert_eq!(step_status(&app, 1), StepStatus::Skipped);
}

#[test]
fn sequence_stale_result_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    let run_gen = runner_gen(&app);
    // A result from a superseded generation must not advance the run.
    app.on_sequence_step(run_gen + 99, 0, Ok(ok_resp(200, "{}")));
    assert_eq!(step_status(&app, 0), StepStatus::Running);
}

#[test]
fn sequence_cancel_marks_pending_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    app.cancel_sequence_run();
    let runner = app.sequence_runner().unwrap();
    assert!(runner.finished);
    assert!(
        runner.steps.iter().all(|s| s.status == StepStatus::Skipped),
        "all non-terminal steps must be skipped on cancel"
    );
}

// ---- unified sequence-runner response viewer ----

/// Opens the sequence runner, lands a JSON `Done` response on step 0, selects
/// it, and focuses the Response region — the shared setup for parity tests.
/// Uses `continue` so a non-200 wouldn't halt (here everything is 200 anyway).
fn seq_app_with_response(root: &Path, body: &str) -> App {
    let mut app = sequence_app(root, "continue", "");
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(json_resp(body)));
    let runner = app.sequence_runner_mut().unwrap();
    runner.selected = 0;
    runner.focus = sequence_runner::RunnerFocus::Response;
    app
}

/// The selected step's live `ResponseView`.
fn seq_view(app: &App) -> &ResponseView {
    match app.sequence_runner().unwrap().selected_response().unwrap() {
        ResponseState::Done { view } => view,
        other => panic!("selected step is not Done: {other:?}"),
    }
}

/// Parity: `p`/`s`/`#`/`W`/`h` toggle the selected STEP's view via the shared
/// handlers, identical to the main pane.
#[test]
fn sequence_response_parity_toggles() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");
    assert_eq!(app.active_response_surface(), ResponseSurface::Sequence);

    assert!(seq_view(&app).pretty());
    app.handle_key(norm('p')).unwrap();
    assert!(!seq_view(&app).pretty(), "p toggled pretty off");
    app.handle_key(norm('p')).unwrap();

    app.handle_key(norm('s')).unwrap();
    assert!(seq_view(&app).sort_keys(), "s toggled sort on");

    assert!(seq_view(&app).line_numbers());
    app.handle_key(norm('#')).unwrap();
    assert!(!seq_view(&app).line_numbers(), "# toggled gutter off");

    app.handle_key(shift('W')).unwrap();
    assert!(seq_view(&app).wrap(), "W toggled wrap on");

    app.handle_key(norm('h')).unwrap();
    assert_eq!(
        seq_view(&app).view_mode(),
        ViewMode::Headers,
        "h switched to headers"
    );
}

/// `/` searches the step response, and Esc returns to the sequence runner (NOT
/// Normal); `y` copies byte-exact raw wire bytes.
#[test]
fn sequence_response_search_and_copy() {
    let dir = tempfile::tempdir().unwrap();
    // Body carries "needle" twice (searchable) plus a tab + NUL that sanitize
    // rewrites on display — so copy proving byte-exactness must return the RAW
    // wire bytes, not the sanitized text (the byte-exactness invariant, in the runner).
    let raw = "needle\tval\u{0}needle";
    let mut app = seq_app_with_response(dir.path(), raw);
    render_once(&mut app);
    app.handle_key(norm('/')).unwrap();
    assert!(matches!(app.mode, Mode::BodySearch));
    assert!(matches!(app.body_search_return, Mode::Sequence { .. }));
    for c in "needle".chars() {
        app.handle_key(norm(c)).unwrap();
    }
    assert!(seq_view(&app).search().is_some_and(|s| s.count() >= 2));
    app.handle_key(keyc(KeyCode::Esc)).unwrap();
    assert!(
        matches!(app.mode, Mode::Sequence { .. }),
        "Esc returns to the sequence runner"
    );

    // Copy returns the raw on-the-wire bytes (tab + NUL intact), not the
    // sanitized display — matching the load-runner copy-invariant test.
    app.handle_key(norm('y')).unwrap();
    assert_eq!(
        app.pending_clipboard.as_ref().unwrap().payload,
        raw,
        "y returns byte-exact raw wire bytes (incl. tab + NUL) in the sequence runner"
    );
}

/// Cursor nav (`j`/`G`) moves the sequence runner's Response cursor via the
/// shared movement path.
#[test]
fn sequence_response_cursor_nav_shared_path() {
    let dir = tempfile::tempdir().unwrap();
    let body = (0..20)
        .map(|i| format!("\"k{i}\":{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut app = seq_app_with_response(dir.path(), &format!("{{{body}}}"));
    render_once(&mut app);
    assert_eq!(app.sequence_runner().unwrap().geometry.cursor, 0);
    app.handle_key(norm('j')).unwrap();
    assert_eq!(app.sequence_runner().unwrap().geometry.cursor, 1);
    app.handle_key(shift('G')).unwrap();
    assert!(app.sequence_runner().unwrap().geometry.cursor > 1);
}

/// PRESERVED NAV: from Response focus, Tab still toggles Steps↔Response and `r`
/// still re-runs — response actions never eat them. And when Steps-focused,
/// `j`/`k` still select steps.
#[test]
fn sequence_runner_nav_not_shadowed() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_app_with_response(dir.path(), "{\"a\":1}");
    assert_eq!(
        app.sequence_runner().unwrap().focus,
        sequence_runner::RunnerFocus::Response
    );
    // Tab toggles Response → Steps (runner-owned; not a response action).
    app.handle_key(keyc(KeyCode::Tab)).unwrap();
    assert_eq!(
        app.sequence_runner().unwrap().focus,
        sequence_runner::RunnerFocus::Steps,
        "Tab still toggles regions"
    );
    // Steps-focused `j`/`k` select steps (not response scroll).
    app.sequence_runner_mut().unwrap().selected = 0;
    app.handle_key(norm('j')).unwrap();
    assert_eq!(
        app.sequence_runner().unwrap().selected,
        1,
        "j selects the next step when Steps-focused"
    );
    // Back to Response; `r` still re-runs (bumps generation).
    app.sequence_runner_mut().unwrap().focus = sequence_runner::RunnerFocus::Response;
    let g0 = runner_gen(&app);
    app.handle_key(norm('r')).unwrap();
    assert!(
        runner_gen(&app) > g0,
        "r still re-runs the sequence from Response focus"
    );
}

/// A response action does nothing when the sequence runner focus is Steps.
#[test]
fn sequence_response_action_inert_when_steps_focused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_app_with_response(dir.path(), "{\"a\":1}");
    app.sequence_runner_mut().unwrap().focus = sequence_runner::RunnerFocus::Steps;
    assert_eq!(app.active_response_surface(), ResponseSurface::Main);
    let before = seq_view(&app).pretty();
    app.handle_key(norm('p')).unwrap();
    assert_eq!(
        seq_view(&app).pretty(),
        before,
        "p is inert when Steps-focused"
    );
}

/// Cross-check: the same scripted scenarios produce identical per-step
/// results + extracted maps through the core `run_sequence` path (real HTTP
/// via wiremock) AND the live TUI driver (`on_sequence_step`) — the guard
/// against the two drifting (the milestone's stated highest risk).
#[tokio::test]
async fn sequence_transition_matches_core() {
    use churl_core::http::{DEFAULT_TIMEOUT, ExecuteOptions, build_client};
    use churl_core::sequence::{RunScopes, StepResult, run_sequence};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // (label, on_error, [statuses], step0 extract expr or "")
    let scenarios: [(&str, OnError, [u16; 3], &str); 4] = [
        ("all-ok+extract", OnError::Halt, [200, 200, 200], "$.token"),
        ("halt-on-500", OnError::Halt, [200, 500, 200], ""),
        ("continue-past-500", OnError::Continue, [200, 500, 200], ""),
        (
            "extract-error-halts",
            OnError::Halt,
            [200, 200, 200],
            "$.missing",
        ),
    ];

    // Maps both result types to a comparable shape (kind tag, status).
    fn norm_core(r: &StepResult) -> (u8, Option<u16>) {
        match r {
            StepResult::Ok { status } => (0, Some(*status)),
            StepResult::Failed { status } => (1, Some(*status)),
            StepResult::HttpError(_) => (2, None),
            StepResult::ExtractError(_) => (3, None),
            StepResult::Skipped => (4, None),
        }
    }
    fn norm_tui(s: &StepStatus) -> (u8, Option<u16>) {
        match s {
            StepStatus::Ok(status) => (0, Some(*status)),
            StepStatus::Failed(status) => (1, Some(*status)),
            StepStatus::HttpError(_) => (2, None),
            StepStatus::ExtractError(_) => (3, None),
            StepStatus::Skipped => (4, None),
            StepStatus::Pending | StepStatus::Running => (9, None),
        }
    }

    for (label, on_error, statuses, extract) in scenarios {
        let server = MockServer::start().await;
        let bodies = [r#"{"token":"T"}"#, "{}", "{}"];
        for (i, status) in statuses.iter().enumerate() {
            Mock::given(method("GET"))
                .and(path(format!("/s{i}")))
                .respond_with(ResponseTemplate::new(*status).set_body_string(bodies[i]))
                .mount(&server)
                .await;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        for i in 0..3 {
            std::fs::write(
                coll.join(format!("s{i}.toml")),
                format!(
                    "seq = 0\nname = \"s{i}\"\n\n[request]\nmethod = \"GET\"\nurl = \"{}/s{i}\"\n",
                    server.uri()
                ),
            )
            .unwrap();
        }
        let extract_block = if extract.is_empty() {
            String::new()
        } else {
            format!("[step.extract]\ntoken = \"{extract}\"\n")
        };
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        std::fs::write(
                seq_dir.join("flow.toml"),
                format!(
                    "seq = 0\nname = \"Flow\"\non_error = \"{}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/s0.toml\"\n{extract_block}\n[[step]]\nseq = 1\nendpoint = \"api/s1.toml\"\n\n[[step]]\nseq = 2\nendpoint = \"api/s2.toml\"\n",
                    match on_error { OnError::Halt => "halt", OnError::Continue => "continue" }
                ),
            )
            .unwrap();

        // --- core path (real HTTP) ---
        let client = build_client(DEFAULT_TIMEOUT).unwrap();
        let file = seq_dir.join("flow.toml");
        let sequence = churl_core::persistence::load_sequence(&file).unwrap();
        let core_run = run_sequence(
            &client,
            root,
            &sequence,
            &RunScopes::default(),
            &ExecuteOptions::default(),
            None,
        )
        .await;
        let core_results: Vec<_> = core_run
            .steps
            .iter()
            .map(|s| norm_core(&s.result))
            .collect();
        let core_extracted = core_run.steps[0].extracted.clone();

        // --- TUI path (injected outcomes matching the mock) ---
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        let seq2 = churl_core::persistence::load_sequence(&file).unwrap();
        app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
            name: seq2.name.clone(),
            file: file.clone(),
            sequence: seq2,
        });
        // Feed each step that actually runs the same response the mock returned.
        while let Some(i) = app.sequence_runner().unwrap().current {
            let g = app.sequence_runner().unwrap().run_generation;
            app.on_sequence_step(g, i, Ok(ok_resp(statuses[i], bodies[i])));
        }
        let runner = app.sequence_runner().unwrap();
        let tui_results: Vec<_> = runner.steps.iter().map(|s| norm_tui(&s.status)).collect();
        let tui_extracted = runner.steps[0].extracted.clone();

        assert_eq!(
            core_results, tui_results,
            "scenario {label}: core vs TUI step results diverged"
        );
        assert_eq!(
            core_extracted, tui_extracted,
            "scenario {label}: core vs TUI extracted maps diverged"
        );
    }
}

/// Builds a sequence app whose step 0 extracts `token` from `$.token`, with the
/// rule's target chosen by `persist` (true = Session, false = Run-only).
fn session_seq_app(root: &Path, persist: bool) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
            coll.join("login.toml"),
            "seq = 0\nname = \"login\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://api.test/login\"\n",
        )
        .unwrap();
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    let persist_line = if persist {
        "persist = [\"token\"]\n"
    } else {
        ""
    };
    std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/login.toml\"\n{persist_line}[step.extract]\ntoken = \"$.token\"\n"
            ),
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let file = seq_dir.join("flow.toml");
    let sequence = churl_core::persistence::load_sequence(&file).unwrap();
    app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
        name: sequence.name.clone(),
        file,
        sequence,
    });
    app
}

/// A dummy SelectedEndpoint for building a standalone resolver in tests.
fn dummy_selected(file: &Path) -> SelectedEndpoint {
    SelectedEndpoint {
        display_path: "api/login".into(),
        file: file.to_owned(),
        collection: 0,
        endpoint: churl_core::persistence::load_endpoint(file).unwrap(),
    }
}

/// Note #6: a Session-target rule writes the extracted value into the workspace
/// session store, and a subsequent STANDALONE send resolves `{{token}}` from it.
#[test]
fn session_rule_persists_and_resolves_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut app = session_seq_app(root, true);
    // Before the run, nothing is captured.
    assert!(app.session_vars().is_empty());
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-123"}"#)));
    // Captured into the in-memory Session store.
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("T-123")
    );
    // A standalone resolver (as a normal send would build) substitutes it.
    let file = root.join("api/login.toml");
    let selected = dummy_selected(&file);
    let resolver = app.build_resolver(&selected);
    assert_eq!(resolver.substitute("Bearer {{token}}"), "Bearer T-123");
}

/// A Run-only rule (default, no `persist`) never writes to the session store.
#[test]
fn run_only_rule_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = session_seq_app(dir.path(), false);
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-123"}"#)));
    assert!(
        app.session_vars().is_empty(),
        "run-only rule must not touch the session store"
    );
}

/// A failed extraction (missing key) never clobbers an existing Session value.
#[test]
fn failed_extraction_keeps_prior_session_value() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut app = session_seq_app(root, true);
    // First run captures T-1.
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-1"}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("T-1")
    );
    // Re-run; this time the body lacks `token` → extraction fails.
    app.start_sequence_run();
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"other":1}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("T-1"),
        "a failed extraction leaves the prior value intact"
    );
}

/// A re-run with a Session-target rule overwrites the captured value.
#[test]
fn session_value_overwritten_on_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut app = session_seq_app(root, true);
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"OLD"}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("OLD")
    );
    app.start_sequence_run();
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"NEW"}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("NEW")
    );
}

/// Cross-workspace isolation: a capture in workspace A does not resolve in B.
#[test]
fn session_captures_are_workspace_isolated() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    // Capture a token in workspace A.
    let mut app = session_seq_app(dir_a.path(), true);
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"A-TOK"}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("A-TOK")
    );
    // Switch the app's workspace to B (same in-memory store, different key).
    std::fs::write(dir_b.path().join("churl.toml"), "name = \"b\"\n").unwrap();
    app.workspace = Some(OpenWorkspace::open(dir_b.path()).unwrap());
    assert!(
        app.session_vars().is_empty(),
        "workspace B sees none of A's captures"
    );
}

/// Security: after a run that persists a token, the session value never
/// touches disk — the sequence/workspace TOMLs are byte-unchanged.
#[test]
fn session_value_never_written_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut app = session_seq_app(root, true);
    let seq_path = root.join("sequences/flow.toml");
    let manifest_path = root.join("churl.toml");
    let endpoint_path = root.join("api/login.toml");
    let seq_before = std::fs::read_to_string(&seq_path).unwrap();
    let manifest_before = std::fs::read_to_string(&manifest_path).unwrap();
    let endpoint_before = std::fs::read_to_string(&endpoint_path).unwrap();

    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"SECRET-TOKEN"}"#)));
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("SECRET-TOKEN")
    );

    // No file carries the captured value; the files are byte-identical.
    assert_eq!(std::fs::read_to_string(&seq_path).unwrap(), seq_before);
    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        manifest_before
    );
    assert_eq!(
        std::fs::read_to_string(&endpoint_path).unwrap(),
        endpoint_before
    );
    // Belt-and-braces: the secret appears in NO file under the workspace.
    for entry in walkdir(root) {
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        assert!(
            !text.contains("SECRET-TOKEN"),
            "session value leaked to disk at {}",
            entry.display()
        );
    }
}

/// Recursively lists every file under `root` (test-only, small trees).
fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn sequence_rerun_resets_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    let run_gen = runner_gen(&app);
    app.on_sequence_step(run_gen, 0, Ok(ok_resp(500, "err")));
    assert!(app.sequence_runner().unwrap().finished);
    app.start_sequence_run();
    let runner = app.sequence_runner().unwrap();
    assert!(!runner.finished);
    assert_eq!(runner.run_generation, run_gen + 1);
    assert_eq!(runner.steps[0].status, StepStatus::Running);
    assert!(
        runner.steps[1..]
            .iter()
            .all(|s| s.status == StepStatus::Pending)
    );
}

/// End-to-end: create a sequence via the prompt, add a step through the
/// picker, toggle on_error, save, and assert the file on disk.
#[test]
fn sequence_editor_create_add_step_save() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
            coll.join("login.toml"),
            "seq = 0\nname = \"login\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://api.test/login\"\n",
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();

    // `<leader>a` with no sequence selected → new-sequence name prompt.
    app.edit_selected_sequence().unwrap();
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewSequence)));
    app.prompt_editor = LineEditor::new("Auth flow");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Edit);

    // `a` opens the picker, Enter accepts the first endpoint.
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    // Toggle on_error to continue, then save with `w`.
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();

    let path = root.join("sequences").join("auth-flow.toml");
    let sequence = churl_core::persistence::load_sequence(&path).unwrap();
    assert_eq!(sequence.name, "Auth flow");
    assert_eq!(sequence.on_error, OnError::Continue);
    assert_eq!(sequence.steps.len(), 1);
    assert_eq!(sequence.steps[0].endpoint, "api/login.toml");
}

/// Two extraction rules with the same name on one step refuse the whole save:
/// the message names the duplicate, nothing is written, the editor stays open.
#[test]
fn sequence_editor_duplicate_rule_names_refuse_save() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("one.toml"),
        "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
    )
    .unwrap();
    // A sequence with one step, no rules yet.
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    let seq_path = seq_dir.join("flow.toml");
    std::fs::write(
            &seq_path,
            "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n",
        )
        .unwrap();
    let before = std::fs::read_to_string(&seq_path).unwrap();

    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let sequence = churl_core::persistence::load_sequence(&seq_path).unwrap();
    app.open_sequence_editor("Flow".to_owned(), seq_path.clone(), &sequence);

    let ch = |app: &mut App, c: char| {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    };
    let enter = |app: &mut App| {
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
    };
    ch(&mut app, 'l'); // focus rules
    // rule "x" = $.a
    ch(&mut app, 'a');
    ch(&mut app, 'x');
    enter(&mut app);
    for c in "$.a".chars() {
        ch(&mut app, c);
    }
    enter(&mut app);
    // second rule, also named "x"
    ch(&mut app, 'a');
    ch(&mut app, 'x');
    enter(&mut app);
    for c in "$.b".chars() {
        ch(&mut app, c);
    }
    enter(&mut app);
    ch(&mut app, 'w'); // save → refused

    assert!(
        matches!(app.mode, Mode::Sequence { .. }),
        "editor stays open on refusal"
    );
    assert_eq!(app.sequence_view().unwrap(), SeqView::Edit);
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("duplicate rule name 'x'")),
        "expected a duplicate-name refusal message, got {:?}",
        app.message.as_ref().map(|m| &m.text)
    );
    // Nothing was written — the file is byte-for-byte unchanged.
    assert_eq!(std::fs::read_to_string(&seq_path).unwrap(), before);
}

// ---- Feature C: unified sequence surface (edit ⇄ run) ----

/// Opens a one-step sequence in the Edit face (via `open_sequence_editor`).
fn sequence_surface_app(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("one.toml"),
        "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
    )
    .unwrap();
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    let seq_path = seq_dir.join("flow.toml");
    std::fs::write(
            &seq_path,
            "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n",
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let sequence = churl_core::persistence::load_sequence(&seq_path).unwrap();
    app.open_sequence_editor("Flow".to_owned(), seq_path, &sequence);
    app
}

fn ctrl_r(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .unwrap();
}

/// Opening a sequence for edit lands on Mode::Sequence { .. } + the Edit face, with
/// no runner built yet (Run is entered lazily).
#[test]
fn sequence_surface_opens_in_edit_face() {
    let dir = tempfile::tempdir().unwrap();
    let app = sequence_surface_app(dir.path());
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Edit);
    assert!(app.sequence_editor().is_some());
    assert!(app.sequence_runner().is_none(), "run built lazily");
}

/// `<leader>s r` (RunSequence) opens the surface in the Run face and starts
/// the run.
#[test]
fn run_sequence_opens_in_run_face() {
    let dir = tempfile::tempdir().unwrap();
    let app = sequence_app(dir.path(), "halt", "");
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Run);
    assert!(app.sequence_runner().is_some());
}

/// Regression: a `<leader>s r` run opens the runner face with NO
/// editor built. `Ctrl-R` must build the editor SYNCHRONOUSLY and land in the
/// Edit face on the flip itself — not leave the surface in a focus-less dead
/// state that only resolves (by exiting to Normal) on the next keypress.
#[test]
fn ctrl_r_from_runner_only_builds_editor_synchronously() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "halt", "");
    // Opened straight into the Run face via the runner path — no editor yet.
    assert_eq!(app.sequence_view().unwrap(), SeqView::Run);
    assert!(
        app.sequence_editor().is_none(),
        "runner-only open builds no editor"
    );
    ctrl_r(&mut app);
    // The flip transferred focus INTO the Edit face immediately.
    assert_eq!(
        app.sequence_view().unwrap(),
        SeqView::Edit,
        "flipped to Edit face"
    );
    assert!(
        app.sequence_editor().is_some(),
        "editor built synchronously on the flip"
    );
    assert!(
        matches!(app.mode, Mode::Sequence { .. }),
        "surface stays open — not exited to Normal"
    );
    // The editor is loaded from the same saved sequence (single source of truth).
    assert_eq!(app.sequence_editor().unwrap().name(), "Flow");
    // And a further keypress is handled by the editor, not a dead-surface exit.
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Sequence { .. }),
        "still in the editor after a key"
    );
}

/// `Ctrl-R` on a CLEAN editor flips to the Run face, building the runner from
/// the saved sequence; a second `Ctrl-R` flips back.
#[test]
fn ctrl_r_toggles_faces_when_clean() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_surface_app(dir.path());
    assert!(!app.sequence_editor().unwrap().is_dirty());
    ctrl_r(&mut app);
    assert_eq!(
        app.sequence_view().unwrap(),
        SeqView::Run,
        "clean edit→run flips"
    );
    let runner = app.sequence_runner().expect("runner built");
    assert_eq!(runner.steps.len(), 1, "runner built from the saved steps");
    // Run→Edit is always safe.
    ctrl_r(&mut app);
    assert_eq!(app.sequence_view().unwrap(), SeqView::Edit);
}

/// A DIRTY editor blocks Edit→Run with a notify (no auto-save, no stale run).
#[test]
fn dirty_edit_to_run_blocks_with_notify() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_surface_app(dir.path());
    // Toggle on_error to make the editor dirty (`o` in the editor).
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
        .unwrap();
    assert!(app.sequence_editor().unwrap().is_dirty());
    ctrl_r(&mut app);
    assert_eq!(
        app.sequence_view().unwrap(),
        SeqView::Edit,
        "stays in edit while dirty"
    );
    assert!(
        app.sequence_runner().is_none(),
        "no runner built while dirty"
    );
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("save (w) before running")),
        "expected a save-first notify, got {:?}",
        app.message.as_ref().map(|m| &m.text)
    );
}

/// `close_sequence_surface` drops both component states + the abort handle and
/// returns to Normal. The editor/runner/face live IN the
/// `Mode::Sequence` payload, so returning to `Mode::Normal` drops all three at
/// once — the accessors go `None` and there is no residual `sequence_view` field
/// to inspect (the fold's whole point).
#[test]
fn close_sequence_surface_clears_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_surface_app(dir.path());
    // Build the runner too (clean flip), so both states are populated.
    ctrl_r(&mut app);
    assert!(app.sequence_runner().is_some());
    assert!(app.sequence_editor().is_some());
    app.close_sequence_surface();
    assert!(matches!(app.mode, Mode::Normal));
    // The whole `Mode::Sequence` payload is gone: no runner, no editor, no face.
    assert!(app.sequence_runner().is_none());
    assert!(app.sequence_editor().is_none());
    assert!(app.sequence_view().is_none());
    assert!(app.sequence_abort.is_none());
}

/// MAJOR: `Ctrl-R` Edit→Run rebuilds the runner but must first ABORT any
/// in-flight step surviving from a prior run (kept alive across Run→Edit) —
/// otherwise the async step (a real POST/DELETE) orphans and runs to
/// completion in the background. Inject a live abort handle and assert the
/// rebuild aborts it.
#[tokio::test]
async fn edit_to_run_aborts_orphaned_inflight_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_surface_app(dir.path());
    // Simulate a prior run whose in-flight step survived a Run→Edit flip: a
    // long-lived task whose abort handle is parked in `sequence_abort`.
    let task = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    });
    let handle = task.abort_handle();
    app.sequence_abort = Some(handle.clone());
    // Force the surface into the Edit face (clean editor) so a Ctrl-R flip
    // takes the Edit→Run rebuild branch. The face lives in the mode.
    if let Mode::Sequence { view, .. } = &mut app.mode {
        *view = SeqView::Edit;
    }
    assert!(!handle.is_finished(), "task live before the flip");

    ctrl_r(&mut app);

    assert_eq!(
        app.sequence_view().unwrap(),
        SeqView::Run,
        "clean edit→run flips"
    );
    assert!(
        app.sequence_abort.is_none(),
        "the prior abort handle was taken during rebuild"
    );
    // Yield so the abort propagates, then confirm the orphaned task is gone.
    tokio::task::yield_now().await;
    assert!(
        handle.is_finished(),
        "the prior in-flight step must be aborted, not orphaned"
    );
}

/// Pressing `f` enters jump-mode; a pane label focuses that pane and exits.
#[test]
fn jump_label_focuses_pane() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Jump));
    assert!(app.jump.is_some());
    // `p` is the Response mnemonic (`s` moved to Sequences,
    // Response took `p` for res`p`onse).
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.jump.is_none());
    assert_eq!(app.focus, Pane::Response);
}

/// `f`-jump labels NO endpoint rows — a row-alphabet key that
/// does not select a row is inert (jump-mode stays open, ignoring it).
#[test]
fn jump_labels_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    assert_eq!(app.explorer.rows().len(), 2);
    let cursor_before = app.explorer.cursor;
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    // `d` was a row label pre-B; now it maps to nothing in jump-mode.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Jump),
        "a non-label key does not exit jump-mode"
    );
    assert_eq!(app.explorer.cursor, cursor_before, "no row was selected");
    assert!(
        app.selected().is_none(),
        "nothing loaded — rows are unlabelled"
    );
}

/// `f` no longer labels a row, so pressing the Jump key again
/// falls through to the "Jump key again cancels" rule.
#[test]
fn jump_f_again_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Jump));
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Normal),
        "f (no longer a label) cancels"
    );
    assert!(app.jump.is_none());
}

/// Esc cancels jump-mode without focusing anything.
#[test]
fn jump_esc_cancels() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    let before = app.focus;
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.jump.is_none());
    assert_eq!(app.focus, before);
}

/// A non-label char in jump-mode is ignored; the mode stays open.
#[test]
fn jump_ignores_unknown_char() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Jump),
        "unknown char must not exit jump-mode"
    );
}

/// `with_config` rejects an unknown profile name, listing the available ones.
#[test]
fn unknown_profile_is_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"demo\"\n\n[[profiles]]\nname = \"dev\"\n",
    )
    .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let result = App::with_config(
        ws,
        KeyMap::default(),
        Theme::default(),
        BTreeMap::new(),
        Some("staging".to_owned()),
    );
    let msg = match result {
        Ok(_) => panic!("expected unknown-profile error"),
        Err(err) => err.to_string(),
    };
    assert!(msg.contains("staging"), "{msg}");
    assert!(msg.contains("dev"), "must list available: {msg}");
}

/// The active profile's vars beat workspace vars in the send-time resolver.
#[test]
fn resolver_precedence_profile_beats_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    // No profile: {{host}} is unresolved (workspace has no `host`), left verbatim.
    let resolver = app.build_resolver(&selected);
    assert_eq!(resolver.substitute("{{host}}"), "{{host}}");
    assert_eq!(resolver.substitute("{{base}}"), "ws");
    // With the prod profile active, {{host}} resolves from the profile.
    app.set_profile(Some("prod".to_owned()));
    let resolver = app.build_resolver(&selected);
    assert_eq!(resolver.substitute("{{host}}"), "prod.test");
}

/// Loads the single `{{host}}` endpoint from `workspace_fixture` into an active
/// buffer (no profile → `host` is unresolved). Shared by the fail-loud send/load
/// tests below.
fn app_with_unresolved_endpoint(root: &Path) -> App {
    let mut app = workspace_fixture(root);
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    app.load_endpoint(selected);
    app
}

/// Fail loud, path 1 (Ctrl-S / main-pane send): a request that still carries an
/// unresolved `{{host}}` after substitution is NOT sent — the message names the
/// variable and no request goes in flight.
#[test]
fn send_refuses_unresolved_variable_with_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_unresolved_endpoint(dir.path());
    app.send_request();
    let msg = app.message.as_ref().map(|m| m.text.as_str());
    assert_eq!(
        msg,
        Some("unresolved variable(s): host — set them in a profile/env or via CLI")
    );
    assert!(
        app.active_endpoint_buffer()
            .is_some_and(|b| b.in_flight.is_none()),
        "no request must go in flight when a variable is unresolved"
    );
    // Sanity: with the `prod` profile active, `host` resolves and the guard does
    // not fire (proves the check is not firing on a resolved request). The client
    // is None so nothing spawns; we clear the stale refusal message first and
    // assert no NEW refusal is set.
    app.set_profile(Some("prod".to_owned()));
    app.message = None;
    app.send_request();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        None,
        "a resolved request must not be refused"
    );
}

/// Fail loud, path 2 (load runner): `open_load_runner` resolves once at open
/// time; an unresolved `{{host}}` refuses to open the runner at all (the whole
/// batch never fires) and names the variable.
#[test]
fn load_runner_refuses_unresolved_variable() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_unresolved_endpoint(dir.path());
    app.open_load_runner();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("unresolved variable(s): host — set them in a profile/env or via CLI")
    );
    assert!(
        app.load_runner().is_none(),
        "the load runner must not open with an unresolved variable"
    );
    assert!(
        app.load_request.is_none(),
        "no request must be armed for the batch"
    );
    assert!(!matches!(app.mode, Mode::LoadRunner(_)));
}

/// Builds a workspace with a single-step sequence whose endpoint request carries
/// `{{missing}}` (no scope resolves it), then opens the runner. `on_error`
/// selects halt vs continue.
fn unresolved_sequence_app(root: &Path, on_error: &str) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    // Step 0's request has an unresolved var; step 1 is clean (to prove continue
    // advances past the failed step).
    std::fs::write(
            coll.join("bad.toml"),
            "seq = 0\nname = \"bad\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{{missing}}\"\n",
        )
        .unwrap();
    std::fs::write(
        coll.join("good.toml"),
        "seq = 0\nname = \"good\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ok\"\n",
    )
    .unwrap();
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"{on_error}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/bad.toml\"\n\n[[step]]\nseq = 1\nendpoint = \"api/good.toml\"\n"
            ),
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let file = seq_dir.join("flow.toml");
    let sequence = churl_core::persistence::load_sequence(&file).unwrap();
    app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
        name: sequence.name.clone(),
        file,
        sequence,
    });
    app
}

/// Fail loud, path 3 (sequence step): a step whose request has an unresolved
/// `{{var}}` fails that step (via `prepare_step`) with a message naming the var,
/// and `on_error = halt` skips the rest.
#[test]
fn sequence_step_fails_on_unresolved_variable_and_halts() {
    let dir = tempfile::tempdir().unwrap();
    let app = unresolved_sequence_app(dir.path(), "halt");
    // Opening the runner drives step 0 synchronously through `prepare_step`,
    // which now refuses the unresolved request.
    match step_status(&app, 0) {
        StepStatus::HttpError(msg) => assert!(
            msg.contains("unresolved variable(s): missing"),
            "step error must name the unresolved variable, got: {msg}"
        ),
        other => panic!("expected HttpError for the unresolved step, got {other:?}"),
    }
    assert_eq!(
        step_status(&app, 1),
        StepStatus::Skipped,
        "halt must skip the remaining step"
    );
    assert!(app.sequence_runner().unwrap().finished);
}

/// Fail loud, path 3 with `on_error = continue`: the unresolved step still fails
/// with the naming message, but the run proceeds to the next (clean) step.
#[test]
fn sequence_step_unresolved_continue_advances() {
    let dir = tempfile::tempdir().unwrap();
    let app = unresolved_sequence_app(dir.path(), "continue");
    assert!(matches!(step_status(&app, 0), StepStatus::HttpError(_)));
    // Continue: step 1 (clean) runs — client is None so it stays Running.
    assert_eq!(step_status(&app, 1), StepStatus::Running);
}

/// Regression: on the Body tab in edtui Normal mode, the
/// row-editing keys (`i` insert, `a` append) must reach edtui instead of
/// being eaten by the Request overlay's RowEdit/RowAdd — there are no rows
/// on the Body tab.
#[test]
fn body_tab_row_keys_reach_edtui() {
    for c in ['i', 'a'] {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        open_bare_endpoint(&mut app);
        app.focus = Pane::Request;
        app.test_tabs().active = RequestTab::Body;
        assert_eq!(app.test_editor().mode, EditorMode::Normal);
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.test_editor().mode,
            EditorMode::Insert,
            "'{c}' on the Body tab must enter edtui insert mode"
        );
        assert!(
            app.test_tabs().editing.is_none(),
            "no row edit must start for '{c}'"
        );
    }
}

/// Regression: after an explorer reload shifts the
/// name-sorted collection indices (a new collection sorting first), the
/// loaded endpoint's collection index is remapped from its file path, so
/// the send-time resolver still reads the *right* collection's
/// `folder.toml` vars.
#[test]
fn reload_remaps_selected_collection_index_for_resolver() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    // One collection "bbb" with folder.toml vars + an endpoint.
    let bbb = dir.path().join("bbb");
    std::fs::create_dir(&bbb).unwrap();
    std::fs::write(bbb.join("folder.toml"), "[vars]\nwho = \"bbb-vars\"\n").unwrap();
    std::fs::write(
        bbb.join("get.toml"),
        "seq = 0\nname = \"Get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{who}}/x\"\n",
    )
    .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    app.load_endpoint(selected);
    // Node 0 is the root collection (M7.9); "bbb" is the first top-level
    // collection, node 1.
    assert_eq!(app.selected().unwrap().collection, 1);

    // Create a collection that sorts *before* "bbb" and reload: name-sorted
    // top-level collections become [aaa=1, bbb=2], so "bbb" shifts to index 2;
    // the stale index 1 would read "aaa"'s (empty) vars.
    churl_core::persistence::create_collection(dir.path(), "aaa", dir.path()).unwrap();
    app.reload_explorer().unwrap();
    assert_eq!(
        app.selected().unwrap().collection,
        2,
        "collection index must be remapped from the file path"
    );
    let selected = app.selected().cloned().unwrap();
    let resolver = app.build_resolver(&selected);
    assert_eq!(
        resolver.substitute("{{who}}"),
        "bbb-vars",
        "resolver must read the loaded endpoint's own collection vars"
    );
}

/// The profile picker sets (and clears) the active profile.
#[test]
fn switch_profile_picker_sets_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::SwitchProfile, None).unwrap();
    assert!(matches!(app.mode, Mode::Palette));
    // Choices: (none), dev, prod.
    assert_eq!(app.picker_profiles().len(), 3);
    if let Some(picker) = app.picker_state_mut() {
        picker.selected = 2; // prod
    }
    app.accept_overlay().unwrap();
    assert_eq!(app.active_profile.as_deref(), Some("prod"));
    // accept drops the whole picker (profiles included) — no residual choices.
    assert!(app.picker_profiles().is_empty());
}

/// The auth-kind picker is its own `Picker::Auth`
/// variant, and accepting a selection swaps the loaded endpoint's `request.auth`
/// to the matching kind. Locks the accept path for the one picker kind that had
/// no coverage before the fold — the index→`Auth` mapping (`set_auth_kind`) is
/// reached only through this variant now.
#[test]
fn auth_kind_picker_accept_sets_request_auth() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    // Load the sole endpoint so `selected()` is populated (the picker refuses to
    // open otherwise).
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    assert!(app.selected().is_some(), "an endpoint is loaded");
    assert!(
        app.selected().unwrap().endpoint.request.auth.is_none(),
        "fixture endpoint starts with no auth"
    );

    // Open the auth-kind picker → it is the `Picker::Auth` variant.
    app.open_auth_kind_picker();
    assert!(matches!(app.mode, Mode::Palette));
    assert!(
        app.picker_is_auth(),
        "the open picker is the auth-kind picker"
    );
    assert_eq!(
        app.picker_state().unwrap().items,
        vec!["None", "Basic", "Bearer", "ApiKey"],
        "labels in index order (index → kind mapping)"
    );

    // Drive selection through the finder (filter to "Bearer") then Enter, mirroring
    // the other picker-accept tests — index 2 = Bearer.
    for c in "Bearer".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal), "accept closes the picker");
    assert!(app.picker.is_none());
    assert!(
        matches!(
            app.selected().unwrap().endpoint.request.auth,
            Some(churl_core::model::Auth::Bearer { .. })
        ),
        "Bearer selection set request.auth to Auth::Bearer"
    );

    // A second index confirms the index→kind mapping: index 3 = ApiKey.
    app.open_auth_kind_picker();
    if let Some(picker) = app.picker_state_mut() {
        picker.selected = 3;
    }
    app.accept_overlay().unwrap();
    assert!(
        matches!(
            app.selected().unwrap().endpoint.request.auth,
            Some(churl_core::model::Auth::ApiKey { .. })
        ),
        "ApiKey selection (index 3) set request.auth to Auth::ApiKey"
    );

    // Esc-cancel leaves the current auth (ApiKey) untouched.
    app.open_auth_kind_picker();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        matches!(
            app.selected().unwrap().endpoint.request.auth,
            Some(churl_core::model::Auth::ApiKey { .. })
        ),
        "Esc-cancel does not change request.auth"
    );
}

/// A newer message replaces the current one in the dedicated message row.
#[test]
fn newer_message_replaces_current() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.notify("first");
    assert_eq!(app.message.as_ref().map(|m| m.text.as_str()), Some("first"));
    app.notify("second");
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("second"),
        "newer message must replace the current one"
    );
}

// ---- leader state machine ----

fn press(app: &mut App, c: char) {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        .unwrap();
}

fn esc(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
}

fn enter(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
}

/// `/` in the help overlay opens a live search; Esc restores (search cleared,
/// help still open); Enter commits (matches retained, input closed).
#[test]
fn help_search_esc_restores_enter_commits() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    press(&mut app, '?');
    assert!(app.help_open, "? opens the help overlay");

    // `/` opens the search input with a live (empty) search.
    press(&mut app, '/');
    assert!(app.help_search_input, "/ opens the help-search input");
    assert!(app.help_search.is_some());

    // Type a query that matches a known label.
    for c in "explorer".chars() {
        press(&mut app, c);
    }
    let count = app.help_search.as_ref().unwrap().count();
    assert!(count > 0, "typing `explorer` matches help rows");

    // Esc restores: search cleared, help still open.
    esc(&mut app);
    assert!(app.help_search.is_none(), "esc clears the help search");
    assert!(!app.help_search_input);
    assert!(app.help_open, "esc keeps the help overlay open");

    // Re-open, type, and Enter commits: matches retained, input closed.
    press(&mut app, '/');
    for c in "explorer".chars() {
        press(&mut app, c);
    }
    enter(&mut app);
    assert!(!app.help_search_input, "enter closes the search input");
    assert!(
        app.help_search.as_ref().map(|s| s.count()).unwrap_or(0) > 0,
        "enter retains the matches for n/N"
    );
    assert!(app.help_open, "enter keeps the help overlay open");

    // `n` still cycles after commit (input closed).
    let before = app.help_search.as_ref().unwrap().current_ordinal();
    press(&mut app, 'n');
    let after = app.help_search.as_ref().unwrap().current_ordinal();
    assert_ne!(before, after, "n cycles matches after enter commits");

    // Closing the overlay clears the search state entirely.
    press(&mut app, '?');
    assert!(!app.help_open);
    assert!(app.help_search.is_none());
}

/// Space enters the root which-key; a direct bind dispatches and dismisses.
#[test]
fn leader_pending_then_dispatch() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    press(&mut app, ' ');
    assert_eq!(app.leader, Some(LeaderState::Root), "space enters root");
    // `<leader>q` quits.
    press(&mut app, 'q');
    assert_eq!(app.leader, None, "dispatch dismisses the popup");
    assert!(app.should_quit);
}

/// An unbound root continuation dismisses the popup with no action; Esc too.
#[test]
fn leader_unbound_and_esc_dismiss() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    press(&mut app, ' ');
    press(&mut app, 'z'); // unbound at root
    assert_eq!(app.leader, None);
    assert!(!app.should_quit);

    press(&mut app, ' ');
    esc(&mut app);
    assert_eq!(app.leader, None, "esc dismisses at root");
}

/// A submenu descent, then a bound key dispatches; Esc backs out one level
/// then cancels; an unknown key cancels at each level.
#[test]
fn leader_submenu_state_machine() {
    let dir = tempfile::tempdir().unwrap();
    // A workspace so RunSequence has something to act on (no panic on empty).
    let mut app = workspace_fixture(dir.path());

    // Space → Root; `s` → Submenu(Sequences).
    press(&mut app, ' ');
    assert_eq!(app.leader, Some(LeaderState::Root));
    press(&mut app, 's');
    assert_eq!(
        app.leader,
        Some(LeaderState::Submenu("sequences".to_owned()))
    );
    // A bound submenu key dispatches and closes the popup.
    press(&mut app, 'r');
    assert_eq!(app.leader, None, "submenu dispatch dismisses");

    // Esc from a submenu backs out to Root; a second Esc cancels.
    press(&mut app, ' ');
    press(&mut app, 's');
    esc(&mut app);
    assert_eq!(
        app.leader,
        Some(LeaderState::Root),
        "esc backs out one level"
    );
    esc(&mut app);
    assert_eq!(app.leader, None, "second esc cancels");

    // Unknown key cancels at Root and inside a submenu.
    press(&mut app, ' ');
    press(&mut app, 'z');
    assert_eq!(app.leader, None);
    press(&mut app, ' ');
    press(&mut app, 's');
    press(&mut app, 'z'); // unbound in the sequences submenu
    assert_eq!(app.leader, None);
}

/// `<leader>e` toggles the explorer sidebar (a direct root bind).
#[test]
fn leader_e_toggles_explorer() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    assert!(!app.explorer_hidden);
    press(&mut app, ' ');
    press(&mut app, 'e');
    assert!(app.explorer_hidden);
    assert_eq!(app.leader, None);
}

/// Leader is inert during text edits: Space types a space in the URL editor,
/// never entering the which-key state.
#[test]
fn leader_inert_during_url_edit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    app.load_endpoint(selected);
    app.begin_url_edit_inline();
    assert!(app.test_url_editor().is_some());
    press(&mut app, ' ');
    assert_eq!(app.leader, None, "space must not enter leader during edit");
    assert!(
        app.test_url_editor().as_ref().unwrap().text().contains(' '),
        "space types a space in the editor"
    );
}

/// Leader is inert in edtui insert mode on the Body tab.
#[test]
fn leader_inert_in_edtui_insert() {
    let mut app = insert_mode_app(KeyMap::default());
    press(&mut app, ' ');
    assert_eq!(app.leader, None);
    assert_eq!(String::from(app.test_editor().lines.clone()), " ");
}

/// The `?` help / `churl keymaps` traversal: the `<leader>s r` chooser
/// (`RunSequencePick`) is reachable only through the sequences submenu, so
/// its combo must render as `s r`. Guards the `leader_combos_for`/`iter_leader`
/// submenu traversal (the help-guard test alone can't catch a missing one).
#[test]
fn run_sequence_shows_submenu_chord() {
    let keymap = KeyMap::default();
    assert_eq!(
        keymap.leader_combos_for(Action::RunSequencePick),
        vec!["s r"]
    );
    assert!(
        keymap
            .iter_leader()
            .any(|(_, a)| a == Action::RunSequencePick),
        "iter_leader must traverse the submenu maps"
    );
}

// ---- digit binds only act in Request ----

/// Global digits do nothing (no pane focus); inside Request they jump tabs.
#[test]
fn digit_focus_removed_at_app_level() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    open_bare_endpoint(&mut app);
    app.focus = Pane::Response;
    press(&mut app, '1'); // was FocusExplorer
    assert_eq!(app.focus, Pane::Response, "digit must not change focus");
    // In the Request pane, 1–4 jump tabs.
    app.focus = Pane::Request;
    press(&mut app, '2');
    assert_eq!(app.test_tabs().active, RequestTab::Headers);
}

// ---- URL→Params merge policy ----

fn params(pairs: &[(&str, &str, bool)]) -> Vec<Param> {
    pairs
        .iter()
        .map(|(n, v, e)| Param {
            name: (*n).to_owned(),
            value: (*v).to_owned(),
            enabled: *e,
        })
        .collect()
}

#[test]
fn merge_rule_a_exact_match_ensures_enabled() {
    // Exact name+value exists but disabled → enabled, no duplicate.
    let mut p = params(&[("a", "1", false)]);
    let report = merge_query_params(&mut p, &[("a".into(), "1".into())]);
    assert_eq!(p.len(), 1);
    assert!(p[0].enabled);
    assert_eq!(report.as_deref(), Some("a updated"));
    // Already enabled + exact → no change, no report.
    let mut p = params(&[("a", "1", true)]);
    assert!(merge_query_params(&mut p, &[("a".into(), "1".into())]).is_none());
    assert_eq!(p.len(), 1);
}

#[test]
fn merge_rule_b_updates_first_row_of_name() {
    let mut p = params(&[("a", "old", false)]);
    let report = merge_query_params(&mut p, &[("a".into(), "new".into())]);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].value, "new");
    assert!(p[0].enabled);
    assert_eq!(report.as_deref(), Some("a updated"));
}

#[test]
fn merge_rule_c_appends_absent() {
    let mut p = params(&[("a", "1", true)]);
    let report = merge_query_params(&mut p, &[("b".into(), "2".into())]);
    assert_eq!(p.len(), 2);
    assert_eq!(p[1].name, "b");
    assert!(p[1].enabled);
    assert_eq!(report.as_deref(), Some("b added"));
}

#[test]
fn merge_rule_d_duplicate_names_positional() {
    // Two existing `tag` rows; `?tag=x&tag=y` maps positionally.
    let mut p = params(&[("tag", "a", true), ("tag", "b", true)]);
    merge_query_params(
        &mut p,
        &[("tag".into(), "x".into()), ("tag".into(), "y".into())],
    );
    assert_eq!(p.len(), 2, "no new rows — positional map");
    assert_eq!(p[0].value, "x");
    assert_eq!(p[1].value, "y");
    // A third duplicate appends (extra).
    let mut p = params(&[("tag", "a", true)]);
    merge_query_params(
        &mut p,
        &[("tag".into(), "x".into()), ("tag".into(), "y".into())],
    );
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].value, "x");
    assert_eq!(p[1].value, "y");
}

#[test]
fn split_query_trims_whitespace() {
    // Names and values are trimmed before decode.
    let (_, pairs) = split_query("https://x/y? a = 1 & b = hello world ");
    assert_eq!(
        pairs,
        vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "hello world".to_owned()),
        ]
    );
    // A key-only segment is trimmed too.
    let (_, pairs) = split_query("https://x/y? flag ");
    assert_eq!(pairs, vec![("flag".to_owned(), String::new())]);
}

#[test]
fn split_query_strips_and_decodes() {
    let (base, pairs) = split_query("https://x/y?a=1&b=hello%20world&c");
    assert_eq!(base, "https://x/y");
    assert_eq!(
        pairs,
        vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "hello world".to_owned()),
            ("c".to_owned(), String::new()),
        ]
    );
    // No query → empty pairs, url unchanged.
    let (base, pairs) = split_query("https://x/y");
    assert_eq!(base, "https://x/y");
    assert!(pairs.is_empty());
}

/// Committing a URL edit strips the query, merges into params, reports the
/// merge, and marks the request dirty.
#[test]
fn commit_url_merges_and_marks_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    app.load_endpoint(selected);
    assert!(!app.is_dirty());
    app.commit_url("https://api.test/users?page=2&sort=name".to_owned());
    let req = app.live_request().unwrap();
    assert_eq!(req.url, "https://api.test/users", "query stripped from URL");
    assert_eq!(req.params.len(), 2);
    assert!(req.params.iter().all(|p| p.enabled));
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("added")),
        "statusline reports the merge"
    );
    assert!(app.is_dirty(), "commit marks the request dirty");
}

// ---- zoom state machine ----

#[test]
fn zoom_toggles_and_restores() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.focus = Pane::Request;
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, Some(ZoomPane::Request));
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, None, "z again restores the split");
    // From Response.
    app.focus = Pane::Response;
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, Some(ZoomPane::Response));
    // No-op from the Explorer/UrlBar.
    app.zoom = None;
    app.focus = Pane::Explorer;
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, None);
}

/// Focusing the collapsed counterpart transfers the zoom to it, so exactly
/// one pane is ever zoomed and a collapsed pane never holds focus (zoom
/// follows focus).
#[test]
fn focus_collapsed_pane_transfers_zoom() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.focus = Pane::Request;
    app.dispatch(Action::Zoom, None).unwrap(); // Response collapsed
    assert_eq!(app.zoom, Some(ZoomPane::Request));
    app.dispatch(Action::FocusResponse, None).unwrap();
    assert_eq!(
        app.zoom,
        Some(ZoomPane::Response),
        "focusing the collapsed pane transfers the zoom to it"
    );
    assert_eq!(app.focus, Pane::Response);
}

/// Zoom follows focus in both directions: zoom Request then focus Response
/// leaves Response zoomed, and the reverse leaves Request zoomed.
#[test]
fn zoom_follows_focus_both_directions() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    // Request zoomed → focus Response ⇒ Response zoomed.
    app.focus = Pane::Request;
    app.zoom = Some(ZoomPane::Request);
    app.set_focus(Pane::Response);
    assert_eq!(app.zoom, Some(ZoomPane::Response));
    assert_eq!(app.focus, Pane::Response);
    // Response zoomed → focus Request ⇒ Request zoomed.
    app.zoom = Some(ZoomPane::Response);
    app.set_focus(Pane::Request);
    assert_eq!(app.zoom, Some(ZoomPane::Request));
    assert_eq!(app.focus, Pane::Request);
}

/// Jumping (jump-mode) into the collapsed pane transfers the zoom too — jump
/// dispatch must go through `set_focus`, not assign focus directly
/// (review round 3, finding #4).
#[test]
fn jump_into_collapsed_pane_transfers_zoom() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.focus = Pane::Response;
    app.dispatch(Action::Zoom, None).unwrap(); // Request collapsed
    assert_eq!(app.zoom, Some(ZoomPane::Response));
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Jump));
    // 'r' is the Request pane mnemonic.
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.focus, Pane::Request);
    assert_eq!(
        app.zoom,
        Some(ZoomPane::Request),
        "jumping into the collapsed pane transfers the zoom to it"
    );
}

// ---- explorer toggle + auto-reopen ----

#[test]
fn explorer_toggle_and_auto_reopen() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.focus = Pane::Request;
    app.dispatch(Action::ToggleExplorer, None).unwrap();
    assert!(app.explorer_hidden);
    // Focusing the explorer auto-reopens it.
    app.dispatch(Action::FocusExplorer, None).unwrap();
    assert!(!app.explorer_hidden, "focus explorer must auto-reopen");
    assert_eq!(app.focus, Pane::Explorer);
    // Hiding while focused moves focus off the explorer.
    app.dispatch(Action::ToggleExplorer, None).unwrap();
    assert!(app.explorer_hidden);
    assert_ne!(app.focus, Pane::Explorer, "hidden pane cannot hold focus");
}

/// Tab cycling skips a collapsed explorer instead of reopening it: it stays
/// hidden and focus lands on the next visible region (both directions).
#[test]
fn tab_skips_hidden_explorer() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.explorer_hidden = true;

    // Forward: Response → (skip Explorer) → UrlBar.
    app.focus = Pane::Response;
    app.dispatch(Action::FocusNext, None).unwrap();
    assert_eq!(app.focus, Pane::UrlBar);
    assert!(app.explorer_hidden, "tab must not reopen the explorer");

    // Backward: UrlBar → (skip Explorer) → Response.
    app.focus = Pane::UrlBar;
    app.dispatch(Action::FocusPrev, None).unwrap();
    assert_eq!(app.focus, Pane::Response);
    assert!(
        app.explorer_hidden,
        "shift-tab must not reopen the explorer"
    );
}

// ---- sequences sub-pane ----

/// A workspace with one collection (one endpoint) and two sequences, for the
/// sub-pane state-machine tests.
fn seq_pane_app(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("one.toml"),
        "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
    )
    .unwrap();
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    for (i, name) in ["Login", "Checkout"].iter().enumerate() {
        std::fs::write(
            seq_dir.join(format!("s{i}.toml")),
            format!("seq = {i}\nname = \"{name}\"\non_error = \"halt\"\n"),
        )
        .unwrap();
    }
    let ws = open_workspace(root).unwrap();
    App::new(ws, KeyMap::default()).unwrap()
}

/// A workspace with one collection (one endpoint) and NO sequences dir, for
/// the focused-empty-pane tests. Mirrors [`seq_pane_app`] minus the
/// `sequences/` dir.
fn empty_seq_pane_app(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("one.toml"),
        "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
    )
    .unwrap();
    let ws = open_workspace(root).unwrap();
    App::new(ws, KeyMap::default()).unwrap()
}

/// The Explorer `s` overlay (`focus-sequences-toggle`) flips
/// `left_active` Endpoints⇄Sequences and focuses the left column. The
/// sub-pane is always present (peek-symmetric), so nothing ever hides.
#[test]
fn focus_sequences_toggle_is_a_focus_switch_never_hides() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "Explorer zoomed default"
    );
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences, "focus switched to it");
    assert_eq!(app.focus, Pane::Explorer);
    // Pressing again switches back — peek-symmetry both directions.
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Endpoints);
}

/// A jump to the Explorer `e` label from a focused Sequences sub-pane must
/// land on the endpoints tree, not stay on Sequences (owner drive-test
/// 2026-07-10). Before the fix, `f e` set focus but never reset
/// `left_active`, so it appeared to do nothing.
#[test]
fn jump_e_lands_on_endpoints_from_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Focus the sequences sub-pane first.
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    // Enter jump-mode and press the Explorer `e` label.
    app.dispatch(Action::Jump, None).unwrap();
    assert!(matches!(app.mode, Mode::Jump));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "`f e` must reach the endpoints tree, not stay on Sequences"
    );
}

/// An EXPLICIT focus on the empty sequences sub-pane STICKS —
/// `set_focus` does not force-revert to Endpoints on a zero-length list.
/// The empty pane is focusable and renders an informative empty
/// state. The reload/switch reconcile path keeps its own guard, exercised by
/// `reload_emptying_sequences_forces_endpoints`.
#[test]
fn explicit_focus_sticks_on_empty_sequences_pane() {
    // A workspace with an endpoint but zero sequences.
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.left_active = LeftPane::Sequences;
    assert_eq!(app.explorer.sequences_len(), 0);
    app.set_focus(Pane::Explorer);
    assert_eq!(
        app.left_active,
        LeftPane::Sequences,
        "an explicit focus on the empty sequences pane is honored (note #3)"
    );
}

/// Note #3: `f s` (the jump `s` label) focuses+zooms the empty sequences
/// sub-pane and it STICKS — the old empty-list invariant no longer reverts it.
#[test]
fn f_jump_s_sticks_on_empty_sequences_pane() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = empty_seq_pane_app(dir.path());
    assert_eq!(app.explorer.sequences_len(), 0);
    app.dispatch(Action::Jump, None).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(
        app.left_active,
        LeftPane::Sequences,
        "`f s` on an empty workspace zooms the sequences pane, not reverts"
    );
}

/// Note #3 — NOT a dead-end: from a focused-empty Sequences pane, `Tab`
/// (FocusNext all the way round) and `f e` both return to the endpoints tree.
#[test]
fn focused_empty_sequences_pane_is_not_a_dead_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = empty_seq_pane_app(dir.path());
    // Zoom into the empty sequences pane.
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    // Way out #1: Tab genuinely LEAVES the left column — the first FocusNext
    // moves focus off Explorer (not a no-op that traps you on the empty pane);
    // a full 4-region cycle then returns to it.
    app.dispatch(Action::FocusNext, None).unwrap();
    assert_ne!(
        app.focus,
        Pane::Explorer,
        "Tab must move focus off the empty sequences pane, not trap on it"
    );
    assert_eq!(app.focus, Pane::UrlBar);
    for _ in 0..3 {
        app.dispatch(Action::FocusNext, None).unwrap();
    }
    assert_eq!(app.focus, Pane::Explorer);
    // Way out #2: `f e` jumps back to the endpoints tree.
    app.dispatch(Action::Jump, None).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "`f e` escapes the empty sequences pane back to endpoints"
    );
    // Way out #3: the `s` overlay toggles straight back too.
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Endpoints);
}

/// Note #3 — the create path works from the focused-empty pane: `<leader>s n`
/// (`NewSequence`) opens the new-sequence prompt (the create entry point) with
/// zero sequences, and none of the in-pane nav keys panic on an empty list.
#[test]
fn empty_sequences_pane_add_and_nav_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = empty_seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    // In-pane nav with an empty list is a no-op — no index-out-of-range panic.
    for action in [
        Action::Down,
        Action::Up,
        Action::Top,
        Action::Bottom,
        Action::Select, // Enter on no selection → new-sequence prompt
    ] {
        app.dispatch(action, None).unwrap();
    }
    // Select (Enter) with no sequence opened the new-sequence name prompt.
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewSequence)));
    assert_eq!(app.explorer.seq_cursor(), 0, "cursor stays pinned at 0");
    assert!(app.explorer.selected_sequence().is_none());

    // `<leader>s n` (NewSequence) is the create entry point (it replaced the
    // buggy `<leader>s a`, which opened the first sequence instead of creating).
    app.mode = Mode::Normal;
    app.dispatch(Action::NewSequence, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Prompt(PromptPurpose::NewSequence)),
        "<leader>s n opens the new-sequence prompt"
    );
    // `r` (RunSequence) with nothing selected is a safe no-op (no panic).
    app.mode = Mode::Normal;
    app.dispatch(Action::RunSequence, None).unwrap();
}

#[test]
fn tab_cycle_never_changes_left_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    // Full Tab cycle back to the explorer must not touch left_active — Tab
    // lands on the left column and RESTORES its last sub-pane (not Endpoints).
    for _ in 0..4 {
        app.dispatch(Action::FocusNext, None).unwrap();
    }
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(
        app.left_active,
        LeftPane::Sequences,
        "Tab is cross-pane only; it restores the last sub-pane"
    );
}

#[test]
fn explorer_nav_routes_to_seq_cursor_when_sequences_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.explorer.seq_cursor(), 0);
    let tree_cursor = app.explorer.cursor;
    // j/Down moves the sequence cursor, not the tree cursor.
    app.dispatch(Action::Down, None).unwrap();
    assert_eq!(app.explorer.seq_cursor(), 1);
    assert_eq!(app.explorer.cursor, tree_cursor, "tree cursor untouched");
    // Enter opens the unified surface (Edit face) on the hovered sequence.
    app.dispatch(Action::Select, None).unwrap();
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Edit);
}

/// `<leader>s r` (`RunSequencePick`) opens a run-flavored chooser and the
/// accepted index RUNS the chosen sequence — not sequence #0. Mirrors the
/// `open_load_runner_pick` tests.
#[test]
fn run_sequence_pick_runs_the_chosen_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    let ordering = app.explorer.all_sequences();
    assert!(ordering.len() >= 2, "fixture has ≥2 sequences");

    // `<leader>s r` opens the picker with the run intent armed.
    app.dispatch(Action::RunSequencePick, None).unwrap();
    assert!(matches!(app.mode, Mode::SequencePicker));
    assert!(app.picker_sequence_runs(), "run intent armed on the picker");

    // Highlight the SECOND sequence and accept it.
    app.picker_state_mut().unwrap().move_down();
    let chosen = ordering[1].0.clone();
    app.accept_overlay().unwrap();

    // The runner opened over the CHOSEN sequence (index 1), not sequences[0].
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Run);
    let runner = app.sequence_runner().expect("runner opened");
    assert_eq!(runner.name, chosen, "ran the chosen sequence, not #0");
    assert!(
        !app.picker_sequence_runs(),
        "one-shot intent cleared after accept"
    );
}

/// `<leader>s o` (`OpenSequencePicker`) still opens the chosen sequence
/// for EDITING — the run intent stays false so the accept path edits.
#[test]
fn open_sequence_pick_edits_not_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::OpenSequencePicker, None).unwrap();
    assert!(matches!(app.mode, Mode::SequencePicker));
    assert!(
        !app.picker_sequence_runs(),
        "edit path: run intent not armed"
    );
    app.accept_overlay().unwrap();
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(
        app.sequence_view().unwrap(),
        SeqView::Edit,
        "picker opens the Edit face"
    );
}

#[test]
fn r_on_sequences_subpane_runs_hovered_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    // `r` maps to Rename, but on the sequences sub-pane it runs the sequence.
    app.dispatch(Action::Rename, None).unwrap();
    assert!(matches!(app.mode, Mode::Sequence { .. }));
    assert_eq!(app.sequence_view().unwrap(), SeqView::Run);
    assert!(app.sequence_runner().is_some());
}

/// `<leader>s <leader>` (Space) is the canonical sequence finder — it opens
/// the sequence picker (edit face), mirroring `<leader><leader>` for
/// endpoints (owner drive-test 2026-07-10).
#[test]
fn leader_s_space_opens_the_sequence_picker() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Space → Root → `s` → sequences submenu → Space.
    press(&mut app, ' ');
    press(&mut app, 's');
    assert_eq!(
        app.leader,
        Some(LeaderState::Submenu("sequences".to_owned()))
    );
    press(&mut app, ' ');
    assert!(
        matches!(app.mode, Mode::SequencePicker),
        "s <leader> opens the sequence picker"
    );
    assert!(
        !app.picker_sequence_runs(),
        "s <leader> is the open/edit finder, not run"
    );
    assert_eq!(app.leader, None, "submenu dispatch dismisses the popup");
}

/// Dynamic submenus, end-to-end at the app layer: a config
/// `g = "+git"` + `[keys.leader.git]` wires a brand-new submenu that shows in
/// which-key and whose keys dispatch through the leader state machine.
#[test]
fn dynamic_leader_submenu_dispatches_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    // A config-defined `git` submenu reached via `<leader>g`, binding `t` to a
    // globally-observable action (toggle the explorer sidebar).
    let global = BTreeMap::new();
    let overlays = BTreeMap::from([
        (
            "leader".to_string(),
            BTreeMap::from([("g".to_string(), "+git".to_string())]),
        ),
        (
            "leader.git".to_string(),
            BTreeMap::from([("t".to_string(), "toggle-explorer".to_string())]),
        ),
    ]);
    let keymap = KeyMap::with_all_overrides(&global, &overlays).unwrap();
    let mut app = App::new(ws, keymap).unwrap();

    // which-key: the root popup lists the `git` submenu behind `g`.
    let (_, root_entries) = leader_popup_entries(&app, &LeaderState::Root);
    assert!(
        root_entries.iter().any(|(k, l)| k == "g" && l == "▸ git"),
        "root which-key shows the config submenu: {root_entries:?}"
    );
    // which-key: inside the submenu, `t` shows its action label.
    let (title, sub_entries) = leader_popup_entries(&app, &LeaderState::Submenu("git".to_owned()));
    assert_eq!(title, " leader · git ");
    assert!(
        sub_entries
            .iter()
            .any(|(k, l)| k == "t" && l == "toggle explorer sidebar"),
        "submenu which-key shows its bind: {sub_entries:?}"
    );

    // Dispatch through the real key path: Space → g → t toggles the explorer.
    let before = app.explorer_hidden;
    press(&mut app, ' ');
    press(&mut app, 'g');
    assert_eq!(app.leader, Some(LeaderState::Submenu("git".to_owned())));
    press(&mut app, 't');
    assert_eq!(app.leader, None, "submenu dispatch dismisses");
    assert_ne!(app.explorer_hidden, before, "the git submenu action ran");
}

/// Deliverable #5 (verify): every "pick"-style leader action opens a picker
/// Mode and never silently runs/executes on entry. Guards against a run-last
/// regression sneaking into any of the four picker front doors.
#[test]
fn pickers_open_a_mode_never_run_last() {
    // Sequence pickers (open + run-pick) both open `Mode::SequencePicker`.
    let d1 = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(d1.path());
    app.dispatch(Action::OpenSequencePicker, None).unwrap();
    assert!(
        matches!(app.mode, Mode::SequencePicker),
        "s o/s f open a picker"
    );
    assert!(app.sequence_runner().is_none(), "no sequence ran on entry");

    let d2 = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(d2.path());
    app.dispatch(Action::RunSequencePick, None).unwrap();
    assert!(
        matches!(app.mode, Mode::SequencePicker),
        "s r opens a picker"
    );
    assert!(app.sequence_runner().is_none(), "no sequence ran on entry");

    // Endpoint request picker (`<leader>f`) opens the search overlay.
    let d3 = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(d3.path());
    app.dispatch(Action::QuickJumpRequests, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Search),
        "<leader>f opens the endpoint picker"
    );

    // Load-runner pick (`<leader>l f`) opens the endpoint picker first,
    // arming the after-pick hand-off — it never fires a load on entry.
    let d4 = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(d4.path());
    app.dispatch(Action::OpenLoadRunnerPick, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Search),
        "<leader>l f opens the endpoint picker"
    );
    assert!(
        app.load_runner().is_none(),
        "no load runner opened on entry"
    );
}

#[test]
fn leader_e_hide_restores_prior_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Focus Response, then Tab-cycle into the explorer (records prior focus).
    app.set_focus(Pane::Response);
    app.set_focus(Pane::Explorer);
    assert_eq!(app.focus, Pane::Explorer);
    // Hide: focus restores to Response (not the URL-bar fallback).
    app.dispatch(Action::ToggleExplorer, None).unwrap();
    assert!(app.explorer_hidden);
    assert_eq!(app.focus, Pane::Response, "true prior-pane restore");
    // Show: re-focus the explorer, landing on the active sub-pane.
    app.left_active = LeftPane::Endpoints; // (default; explicit for clarity)
    app.dispatch(Action::ToggleExplorer, None).unwrap();
    assert!(!app.explorer_hidden);
    assert_eq!(app.focus, Pane::Explorer);
}

#[test]
fn leader_e_hide_falls_back_to_urlbar_without_prior() {
    // No recorded prior pane (explorer focused from the start) → URL bar.
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    assert_eq!(app.focus, Pane::Explorer);
    app.dispatch(Action::ToggleExplorer, None).unwrap();
    assert!(app.explorer_hidden);
    assert_eq!(app.focus, Pane::UrlBar);
}

#[test]
fn z_still_noop_from_explorer() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(app.zoom, None);
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, None, "z zooms only Request/Response");
}

/// B1 regression: with the sequences sub-pane focused, `d`/`N` must NOT open
/// a tree delete/new-collection prompt targeting the off-screen tree cursor
/// (which sits on collection "api"); `n` opens the new-SEQUENCE prompt, not a
/// new-endpoint one. Fails pre-fix (d→delete-collection confirm, n→new
/// endpoint under the invisible collection).
#[test]
fn tree_mutating_keys_suppressed_on_sequences_subpane() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Put the tree cursor on the collection so `d`/`n`/`N` would target it.
    app.explorer.cursor = 0;
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert!(app.left_column_on_sequences());

    // `d` → the delete-SEQUENCE confirm (never the tree delete prompt).
    app.dispatch(Action::Delete, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Confirm(ConfirmPurpose::DeleteSequence)),
        "d on the sequences sub-pane confirms a sequence delete, not a tree delete"
    );
    // Back out so the rest of the assertions run from Normal.
    app.dispatch(Action::Cancel, None).ok();
    app.mode = Mode::Normal;

    // `N` → no new-collection prompt; mode stays Normal.
    app.dispatch(Action::NewCollection, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Normal),
        "N must not open a new-collection prompt"
    );

    // `n` → the new-SEQUENCE prompt (not new-endpoint).
    app.dispatch(Action::NewEndpoint, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Prompt(PromptPurpose::NewSequence)),
        "n on the sequences sub-pane creates a new sequence"
    );
}

/// Note #6: `d` → confirm → `y` on the sequences sub-pane deletes the hovered
/// sequence file, refreshes the pane, and the sub-pane cursor lands on a
/// surviving sequence. Mirrors the endpoint-delete confirm flow.
#[test]
fn d_confirm_y_deletes_hovered_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert!(app.left_column_on_sequences());
    assert_eq!(app.explorer.sequences_len(), 2);

    // Cursor 0 → "Login" (seq = 0). Capture its file before deleting.
    let selected = app.explorer.selected_sequence().unwrap();
    assert_eq!(selected.name, "Login");
    let deleted_path = selected.file.clone();
    assert!(deleted_path.exists());

    // `d` opens the confirm, `y` performs the delete.
    app.dispatch(Action::Delete, None).unwrap();
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DeleteSequence)
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .unwrap();

    assert!(matches!(app.mode, Mode::Normal), "confirm closes on y");
    assert!(!deleted_path.exists(), "sequence file removed from disk");
    assert_eq!(app.explorer.sequences_len(), 1, "pane refreshed");
    // Selection lands sensibly on the surviving sequence.
    assert_eq!(app.explorer.selected_sequence().unwrap().name, "Checkout");
}

/// Note #6, empty-after-delete case: deleting the last remaining sequence
/// leaves an empty sub-pane and reconciles the left-column focus back to the
/// endpoints tree (never stranded on an empty sub-pane).
#[test]
fn deleting_last_sequence_empties_pane_and_reconciles_focus() {
    let dir = tempfile::tempdir().unwrap();
    // A workspace with exactly one sequence.
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("one.toml"),
        "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
    )
    .unwrap();
    let seq_dir = dir.path().join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    std::fs::write(
        seq_dir.join("only.toml"),
        "seq = 0\nname = \"Only\"\non_error = \"halt\"\n",
    )
    .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();

    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    assert_eq!(app.explorer.sequences_len(), 1);
    let only = app.explorer.selected_sequence().unwrap().file;

    app.dispatch(Action::Delete, None).unwrap();
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DeleteSequence)
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .unwrap();

    assert!(!only.exists(), "last sequence file removed");
    assert_eq!(app.explorer.sequences_len(), 0, "pane now empty");
    assert!(app.explorer.selected_sequence().is_none());
    // Focus reconciled off the empty sub-pane.
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "focus reconciles to endpoints when the sequence list empties"
    );
}

/// Note #6: `d` on the sequences sub-pane with no sequence selected notifies
/// and never opens a confirm.
#[test]
fn d_on_empty_sequences_subpane_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = empty_seq_pane_app(dir.path());
    app.left_active = LeftPane::Sequences;
    app.focus = Pane::Explorer;
    assert!(app.explorer.selected_sequence().is_none());

    app.dispatch(Action::Delete, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Normal),
        "no confirm when nothing is selected"
    );
}

/// The same tree-mutating keys keep their normal behaviour on the endpoints
/// tree (guard is scoped to `left_active==Sequences`).
#[test]
fn tree_mutating_keys_work_normally_on_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Sub-pane shown but endpoints active (the default occupant).
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    app.dispatch(Action::FocusSequencesToggle, None).unwrap(); // → Endpoints
    assert_eq!(app.left_active, LeftPane::Endpoints);
    app.explorer.cursor = 0; // on collection "api"

    app.dispatch(Action::Delete, None).unwrap();
    assert!(
        matches!(
            app.mode,
            Mode::Prompt(PromptPurpose::DeleteCollectionConfirm)
        ),
        "d still deletes the selected collection on the endpoints tree"
    );
}

/// Regression: switching from a sequenced workspace (sub-pane on, sequences
/// focused) into a sequence-less one resets to endpoints — focus is never
/// stranded on an empty sub-pane and B1 is disarmed. Fails pre-fix
/// (`left_active` stays `Sequences` with an empty list).
#[test]
fn workspace_switch_into_sequenceless_resets_to_endpoints() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir_a.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);

    // A second workspace with NO sequences.
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("churl.toml"), "name = \"empty\"\n").unwrap();
    let coll = dir_b.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("x.toml"),
        "seq = 0\nname = \"x\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/x\"\n",
    )
    .unwrap();

    app.switch_workspace(dir_b.path().to_path_buf()).unwrap();
    assert_eq!(app.explorer.sequences_len(), 0);
    // A workspace switch is a clean slate: `switch_workspace` always lands on
    // the endpoints tree (it sets `left_active = Endpoints` directly), so the
    // old workspace's Sequences focus never carries into the new one.
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "focus must not strand on an empty sequences pane"
    );
    assert!(!app.left_column_on_sequences(), "B1 disarmed");
}

/// Regression (reload path): the invariant forces Endpoints when a reload
/// empties the sequence list even without a full workspace switch.
#[test]
fn reload_emptying_sequences_forces_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    // Delete every sequence file on disk, then reload.
    std::fs::remove_dir_all(dir.path().join("sequences")).unwrap();
    app.reload_explorer().unwrap();
    assert_eq!(app.explorer.sequences_len(), 0);
    assert_eq!(
        app.left_active,
        LeftPane::Endpoints,
        "an emptied sub-pane never keeps focus"
    );
}

// ---- 4-region Tab, cycle-region, f-jump, hover-fallback ----

/// B1: Tab cycles the 4 regions Explorer→UrlBar→Request→Response→Explorer, and
/// landing back on the left column RESTORES its last sub-pane (Sequences),
/// never force-resetting to Endpoints.
#[test]
fn tab_cycles_four_regions_and_restores_left_sub_pane() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Put the left column on Sequences, then tab away and all the way back.
    app.dispatch(Action::FocusSequencesToggle, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    assert_eq!(app.focus, Pane::Explorer);
    let order = [Pane::UrlBar, Pane::Request, Pane::Response, Pane::Explorer];
    for expected in order {
        app.dispatch(Action::FocusNext, None).unwrap();
        assert_eq!(app.focus, expected);
    }
    assert_eq!(
        app.left_active,
        LeftPane::Sequences,
        "landing on the left column restores its last sub-pane, not Endpoints"
    );
}

/// B1: Tab into the left column restores the Request/Response zoom too — the
/// zoom-follows-focus invariant survives the 4-region model.
#[test]
fn tab_restores_request_response_zoom() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    // Zoom Response, then Tab forward one step: Response→Explorer keeps the
    // Response zoom parked (it only transfers between Request⇄Response).
    app.set_focus(Pane::Response);
    app.dispatch(Action::Zoom, None).unwrap();
    assert_eq!(app.zoom, Some(ZoomPane::Response));
    app.dispatch(Action::FocusNext, None).unwrap(); // → Explorer
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(app.zoom, Some(ZoomPane::Response), "zoom parked across Tab");
    // Tab to Request: zoom-follows-focus flips it to Request (collapsed pane
    // never holds focus).
    app.dispatch(Action::FocusNext, None).unwrap(); // → UrlBar
    app.dispatch(Action::FocusNext, None).unwrap(); // → Request
    assert_eq!(app.focus, Pane::Request);
    assert_eq!(app.zoom, Some(ZoomPane::Request));
}

/// B2: `cycle-region-fwd`/`back` cycles the left column's sub-panes when the
/// left column is focused (Endpoints⇄Sequences), and the buffer/tab ring when
/// a right-column pane is focused.
#[test]
fn cycle_region_switches_sub_pane_on_left_column() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    app.set_focus(Pane::Explorer);
    assert_eq!(app.left_active, LeftPane::Endpoints);
    app.dispatch(Action::CycleRegionFwd, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Sequences);
    app.dispatch(Action::CycleRegionBack, None).unwrap();
    assert_eq!(app.left_active, LeftPane::Endpoints);
}

/// B2: `cycle-region` ships with NO default key binding (Ctrl-Tab is
/// deliberately not hardcoded — terminal-unreliable). It must be unbound in
/// the global map, every pane overlay AND the leader tree.
#[test]
fn cycle_region_is_unbound_by_default() {
    let keymap = KeyMap::default();
    for action in [Action::CycleRegionFwd, Action::CycleRegionBack] {
        assert!(
            keymap.combos_for(action).is_empty(),
            "{action:?} must have no global default"
        );
        assert!(
            keymap.leader_combos_for(action).is_empty(),
            "{action:?} must have no leader default"
        );
        for ctx in [
            PaneCtx::Explorer,
            PaneCtx::UrlBar,
            PaneCtx::Request,
            PaneCtx::Response,
        ] {
            assert!(
                keymap.overlay_combos_for(ctx, action).is_empty(),
                "{action:?} must have no {ctx:?} overlay default"
            );
        }
    }
}

/// B4: `f`-jump labels the five pane regions only (no rows) and the Sequences
/// label focuses the left column on the sequences sub-pane.
#[test]
fn f_jump_reaches_the_sequences_pane() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Enter jump-mode and verify the five region labels are assigned.
    app.dispatch(Action::Jump, None).unwrap();
    let jump = app.jump.as_ref().expect("jump-mode active");
    assert_eq!(jump.label_for_pane(Pane::Explorer), Some('e'));
    assert_eq!(jump.label_for_sequences(), Some('s'));
    assert_eq!(jump.label_for_pane(Pane::UrlBar), Some('u'));
    assert_eq!(jump.label_for_pane(Pane::Request), Some('r'));
    assert_eq!(jump.label_for_pane(Pane::Response), Some('p'));
    // Pressing `s` jumps to the sequences sub-pane.
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Normal),
        "a label jump closes jump-mode"
    );
    assert_eq!(app.focus, Pane::Explorer);
    assert_eq!(app.left_active, LeftPane::Sequences);
}

/// A workspace with one collection holding two endpoints (distinct URLs), for
/// the hover-vs-selection fallback tests.
fn hover_fallback_ws(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
            coll.join("a.toml"),
            "seq = 0\nname = \"alpha\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/alpha\"\n",
        )
        .unwrap();
    std::fs::write(
            coll.join("b.toml"),
            "seq = 1\nname = \"beta\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/beta\"\n",
        )
        .unwrap();
    let ws = open_workspace(root).unwrap();
    App::new(ws, KeyMap::default()).unwrap()
}

/// B5: copy-as-curl falls back to the HOVERED endpoint when nothing is loaded.
#[test]
fn copy_as_curl_falls_back_to_hovered_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = hover_fallback_ws(dir.path());
    // Hover the first endpoint with nothing loaded.
    app.explorer.expand().unwrap();
    app.explorer.move_down(); // onto "alpha"
    app.set_focus(Pane::Explorer);
    assert!(app.selected().is_none(), "no buffer loaded");
    assert!(
        app.hovered_endpoint().is_some(),
        "cursor hovers an endpoint"
    );
    app.copy_as_curl(false);
    let job = app.pending_clipboard.as_ref().expect("copy enqueued a job");
    assert!(
        job.payload.contains("api.test/alpha"),
        "copy fell back to the hovered endpoint's request: {}",
        job.payload
    );
}

/// B5: with an endpoint LOADED, copy-as-curl prefers the loaded one even when
/// the explorer cursor hovers a different endpoint.
#[test]
fn copy_as_curl_prefers_loaded_over_hover() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = hover_fallback_ws(dir.path());
    app.explorer.expand().unwrap();
    app.explorer.move_down(); // onto "alpha"
    // Load "alpha" into a buffer.
    let alpha = app.explorer.select().unwrap().expect("alpha selected");
    app.load_endpoint(alpha);
    assert!(app.selected().is_some(), "alpha loaded");
    // Move the cursor onto "beta" so the hover differs from the load.
    app.set_focus(Pane::Explorer);
    app.explorer.move_down(); // onto "beta"
    assert!(
        app.hovered_endpoint()
            .is_some_and(|h| h.endpoint.request.url.contains("beta")),
        "cursor now hovers beta"
    );
    app.copy_as_curl(false);
    let job = app.pending_clipboard.as_ref().expect("copy enqueued a job");
    assert!(
        job.payload.contains("api.test/alpha") && !job.payload.contains("api.test/beta"),
        "copy prefers the loaded endpoint (alpha) over the hover (beta): {}",
        job.payload
    );
}

/// m1 regression: `<leader>s o` picking a sequence moves the sub-pane cursor
/// onto it, so a later run/edit acts on the PICKED sequence, not #0.
#[test]
fn picking_a_sequence_sets_seq_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = seq_pane_app(dir.path());
    // Sequences sort by seq: "Login" (0), "Checkout" (1). Pick #1's file.
    let (_, checkout_file) = app
        .explorer
        .all_sequences()
        .into_iter()
        .find(|(name, _)| name == "Checkout")
        .unwrap();
    app.open_picked_sequence(checkout_file).unwrap();
    assert_eq!(app.explorer.seq_cursor(), 1);
    assert_eq!(app.explorer.selected_sequence().unwrap().name, "Checkout");
}

// ---- URL popup editor ----

fn app_with_endpoint(dir: &Path) -> App {
    let mut app = workspace_fixture(dir);
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    let selected = app.explorer.select().unwrap().expect("endpoint");
    app.load_endpoint(selected);
    app
}

#[test]
fn url_popup_commit_runs_merge() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_popup();
    assert!(app.test_url_popup().is_some());
    *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://api.test/x?q=1")));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(app.test_url_popup().is_none(), "enter commits + closes");
    let req = app.live_request().unwrap();
    assert_eq!(req.url, "https://api.test/x");
    assert!(req.params.iter().any(|p| p.name == "q" && p.enabled));
}

#[test]
fn url_popup_esc_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    let before = app.live_request().unwrap().url.clone();
    app.begin_url_popup();
    // In Normal mode, Esc cancels (edtui popups open in Normal mode).
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(app.test_url_popup().is_none());
    assert_eq!(app.live_request().unwrap().url, before, "url unchanged");
}

#[test]
fn url_popup_single_line_constraint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_popup();
    // A buffer that somehow holds two lines collapses to one on commit.
    *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://a/b\nc")));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.live_request().unwrap().url, "https://a/bc");
}

/// `/`-search in the popup executes on Enter (jump to match → Normal), and
/// the popup stays open; a second Enter commits. Regression: `handle_url_popup_key`
/// must not commit on any Enter, or Search could never run.
#[test]
fn url_popup_search_executes_then_commits() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_popup();
    *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://api.test/find")));
    // `/` enters Search; type "find"; Enter runs FindFirst → Normal.
    for c in "/find".chars() {
        press(&mut app, c);
    }
    assert_eq!(
        app.test_url_popup().as_ref().unwrap().mode,
        EditorMode::Search
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    let popup = app.test_url_popup().as_ref().expect("popup still open");
    assert_eq!(popup.mode, EditorMode::Normal, "search left Search mode");
    assert_eq!(popup.cursor.col, 17, "cursor jumped to the 'find' match");
    // A second Enter (now in Normal) commits.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(app.test_url_popup().is_none(), "second enter commits");
}

/// The vim motion extensions move the popup cursor in Normal mode.
#[test]
fn url_popup_vim_motions_move_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_popup();
    *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("foo bar baz")));
    let cursor = |app: &App| app.test_url_popup().as_ref().unwrap().cursor.col;

    press(&mut app, 'W'); // → start of "bar"
    assert_eq!(cursor(&app), 4);
    press(&mut app, 'B'); // → back to "foo"
    assert_eq!(cursor(&app), 0);
    press(&mut app, 'f'); // find-char forward…
    press(&mut app, 'z');
    assert_eq!(cursor(&app), 10); // the 'z' in "baz"
    press(&mut app, 'F'); // find-char backward…
    press(&mut app, 'o');
    assert_eq!(cursor(&app), 2); // last 'o' before the cursor
    press(&mut app, '^'); // first non-blank of the row
    assert_eq!(cursor(&app), 0);
}

/// Esc while an f/F/t/T find is pending aborts the find (vim) — it must not
/// close the popup. A second Esc (no pending) cancels as usual.
#[test]
fn url_popup_esc_aborts_pending_find_not_popup() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_popup();
    *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("foo bar")));
    press(&mut app, 'f'); // pending find…
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(
        app.test_url_popup().is_some(),
        "esc aborted the find, not the popup"
    );
    // The find is gone: a char is typed-through to edtui, not a target.
    press(&mut app, 'b');
    assert_eq!(
        app.test_url_popup().as_ref().unwrap().cursor.col,
        0,
        "aborted find must not resolve on the next char"
    );
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(
        app.test_url_popup().is_none(),
        "esc with no pending cancels"
    );
}

/// Body tab in Normal mode: `W` moves the editor cursor, and `f`+char is
/// find-char inside the editor — it does NOT open jump-mode.
#[test]
fn body_tab_vim_motions_and_f_shadows_jump() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    open_bare_endpoint(&mut app);
    app.focus = Pane::Request;
    app.test_tabs().active = RequestTab::Body;
    *app.test_editor() = EditorState::new(Lines::from("foo bar baz"));
    app.test_editor().mode = EditorMode::Normal;

    press(&mut app, 'W');
    assert_eq!(app.test_editor().cursor.col, 4, "W moved the body cursor");

    press(&mut app, 'f');
    press(&mut app, 'z');
    assert_eq!(
        app.test_editor().cursor.col,
        10,
        "f<c> moved the body cursor"
    );
    assert!(app.jump.is_none(), "f shadowed jump inside the Body editor");
    assert_eq!(app.test_editor().mode, EditorMode::Normal);
}

/// The `f` shadow is Body-scoped: from the Explorer pane `f` still enters
/// jump-mode (guard is per-pane/tab).
#[test]
fn f_from_explorer_still_enters_jump() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.focus = Pane::Explorer;
    press(&mut app, 'f');
    assert!(app.jump.is_some(), "jump-mode still reachable outside Body");
}

#[test]
fn url_edit_mode_selects_inline_vs_popup() {
    let dir = tempfile::tempdir().unwrap();
    // Default (inline): begin_url_edit opens the inline editor.
    let mut app = app_with_endpoint(dir.path());
    app.begin_url_edit();
    assert!(app.test_url_editor().is_some());
    assert!(app.test_url_popup().is_none());
    // Popup mode: begin_url_edit opens the popup.
    let dir2 = tempfile::tempdir().unwrap();
    let mut app = app_with_endpoint(dir2.path());
    app.set_url_edit_mode(UrlEditMode::Popup);
    app.begin_url_edit();
    assert!(app.test_url_popup().is_some());
    assert!(app.test_url_editor().is_none());
}

// ---- help overlay ----

#[test]
fn help_opens_and_closes() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.dispatch(Action::Help, None).unwrap();
    assert!(app.help_open);
    press(&mut app, 'j');
    assert_eq!(app.help_scroll, 1);
    press(&mut app, 'k');
    assert_eq!(app.help_scroll, 0);
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
        .unwrap();
    assert!(!app.help_open);
}

/// The profile picker marks the active profile with ● and (none) with ●
/// when no profile is active.
#[test]
fn profile_picker_marks_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    // No active profile: (none) is marked.
    app.dispatch(Action::SwitchProfile, None).unwrap();
    let picker = app.picker_state().unwrap();
    assert_eq!(picker.items[0], "● (none)");
    assert_eq!(picker.items[1], "dev");
    assert_eq!(picker.items[2], "prod");

    // Fresh fixture with dev active, to keep state clean.
    let dir2 = tempfile::tempdir().unwrap();
    let mut app2 = workspace_fixture(dir2.path());
    app2.active_profile = Some("dev".to_owned());
    app2.dispatch(Action::SwitchProfile, None).unwrap();
    let picker2 = app2.picker_state().unwrap();
    assert_eq!(picker2.items[0], "(none)");
    assert_eq!(picker2.items[1], "● dev");
    assert_eq!(picker2.items[2], "prod");
}

// ---- quick-jump pickers ----

/// A second workspace fixture with a distinctly-named collection, so a
/// workspace switch is observable in the explorer tree.
fn other_workspace_fixture(root: &Path) {
    std::fs::write(root.join("churl.toml"), "name = \"other\"\n").unwrap();
    let coll = root.join("orders");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
            coll.join("create.toml"),
            "seq = 0\nname = \"Create order\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://x/orders\"\n",
        )
        .unwrap();
}

/// `<leader><leader>` (Space Space) reuses the endpoint-search overlay as
/// the request picker (owner drive-test 2026-07-10 — moved off `<leader>f`).
#[test]
fn leader_leader_opens_request_picker() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(
        app.leader,
        Some(LeaderState::Root),
        "space enters the root which-key state"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Search),
        "<leader><leader> opens the search overlay"
    );
    let picker = app.picker_state().expect("picker open");
    assert!(
        picker.items.iter().any(|i| i.contains("Get user")),
        "request picker lists the workspace endpoints: {:?}",
        picker.items
    );
}

/// `<leader>w` with no history store shows a message, not an empty picker.
#[test]
fn workspace_picker_empty_without_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::QuickJumpWorkspaces, None).unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.picker.is_none());
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("no recent workspaces")
    );
}

/// `<leader>w` opens the workspace picker over the recency list (newest
/// first), storing canonical paths index-aligned with the display items.
#[test]
fn workspace_picker_lists_recent() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let store = HistoryStore::in_memory().unwrap();
    store.touch_workspace("/ws/alpha", 1_000).unwrap();
    store.touch_workspace("/ws/beta", 2_000).unwrap();
    app.history = Some(store);

    app.dispatch(Action::QuickJumpWorkspaces, None).unwrap();
    assert!(matches!(app.mode, Mode::WorkspacePicker));
    let picker = app.picker_state().expect("picker open");
    assert_eq!(picker.items, vec!["/ws/beta", "/ws/alpha"]);
    assert_eq!(
        app.picker_workspaces(),
        vec![PathBuf::from("/ws/beta"), PathBuf::from("/ws/alpha")]
    );
}

/// Switching workspaces rebuilds the explorer against the new tree and resets
/// every endpoint/workspace-scoped field (a stale one would be a correctness
/// bug). Also records the switch in the recency table.
#[test]
fn switch_workspace_resets_state_and_loads_new_tree() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir_a.path());
    app.history = Some(HistoryStore::in_memory().unwrap());

    // Load an endpoint from A and make the app "busy": dirty editor, response
    // pane focus, an active profile, a non-Idle response.
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    assert!(app.selected().is_some());
    *app.test_editor() = EditorState::new(Lines::from("dirty body"));
    app.active_profile = Some("dev".to_owned());
    app.focus = Pane::Response;
    *app.response_mut() = ResponseState::Cancelled;

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());
    let path_b = dir_b.path().to_path_buf();

    app.switch_workspace(path_b.clone()).unwrap();

    // Workspace + explorer now reflect B.
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
    let names: Vec<String> = app.explorer.rows().iter().map(|r| r.name.clone()).collect();
    assert!(names.iter().any(|n| n == "orders"), "shows B: {names:?}");
    assert!(!names.iter().any(|n| n == "users"), "no A rows: {names:?}");

    // Endpoint/workspace-scoped state is reset: the buffer (which owns the
    // editor/response/dirty state) is dropped entirely.
    assert!(app.selected().is_none());
    assert!(
        app.test_no_snapshot(),
        "buffer dropped → editor/response gone"
    );
    assert!(app.active_profile.is_none());
    assert_eq!(app.explorer.cursor, 0);
    assert_eq!(app.focus, Pane::Explorer);
    assert!(matches!(app.response(), ResponseState::Idle));
    assert!(!app.is_dirty(), "no dirty state after switch");
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("switched to other")
    );

    // Recency recorded the switch (canonical path of B).
    let recent = app.history.as_ref().unwrap().recent_workspaces(10).unwrap();
    let canon = canonical_path(&path_b).to_string_lossy().into_owned();
    assert!(recent.contains(&canon), "recency has B: {recent:?}");
}

/// A dirty switch defers to the discard-changes confirm (workspace targets
/// are always "other"); discarding then performs the switch.
#[test]
fn dirty_workspace_switch_defers_to_confirm() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir_a.path());
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    *app.test_editor() = EditorState::new(Lines::from("unsaved edit"));
    assert!(app.is_dirty());

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());

    app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
        .unwrap();
    // Deferred: confirm overlay open, switch not yet performed.
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges)
    ));
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "demo");

    // Discard: the switch goes through.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
    assert!(app.selected().is_none());
}

/// A two-endpoint workspace whose files exist on disk, so `save_request`
/// (which writes TOML) succeeds. Opens both endpoints as buffers (`alpha`
/// active last).
fn two_endpoint_app(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    for name in ["alpha", "beta"] {
        std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
    }
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.guarded_load(PendingLoad::File(coll.join("beta.toml")))
        .unwrap();
    app.guarded_load(PendingLoad::File(coll.join("alpha.toml")))
        .unwrap();
    app
}

/// A workspace switch with MULTIPLE dirty buffers. `s` must save EVERY
/// dirty buffer (not just the active one) before the switch destroys them all
/// — a non-active dirty buffer must never be lost silently.
#[test]
fn workspace_switch_s_saves_all_dirty_buffers() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = two_endpoint_app(dir_a.path());
    app.history = Some(HistoryStore::in_memory().unwrap());
    // Dirty BOTH buffers via a saveable body edit.
    let alpha_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
        .unwrap();
    let beta_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
        .unwrap();
    app.active = beta_idx;
    *app.test_editor() = EditorState::new(Lines::from("beta-body"));
    app.active = alpha_idx;
    *app.test_editor() = EditorState::new(Lines::from("alpha-body"));
    assert!(app.any_buffer_dirty());

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());
    app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges)),
        "multi-dirty switch reaches the confirm"
    );

    // `s`: save all, then switch (all clean).
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
    // BOTH files were written — the non-active buffer (beta) was not lost.
    let alpha = std::fs::read_to_string(dir_a.path().join("api/alpha.toml")).unwrap();
    let beta = std::fs::read_to_string(dir_a.path().join("api/beta.toml")).unwrap();
    assert!(alpha.contains("alpha-body"), "active saved: {alpha}");
    assert!(
        beta.contains("beta-body"),
        "the NON-active dirty buffer was saved too: {beta}"
    );
}

/// The workspace-switch guard fires even when ONLY a NON-active buffer is
/// dirty (the guard uses `any_buffer_dirty`, not active-only).
#[test]
fn workspace_switch_guards_on_nonactive_dirty() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = two_endpoint_app(dir_a.path());
    app.history = Some(HistoryStore::in_memory().unwrap());
    // Dirty ONLY the non-active buffer (beta); leave the active one (alpha)
    // clean.
    let beta_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
        .unwrap();
    let alpha_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
        .unwrap();
    app.active = beta_idx;
    *app.test_editor() = EditorState::new(Lines::from("beta-edit"));
    app.active = alpha_idx;
    assert!(!app.is_dirty(), "active (alpha) is clean");
    assert!(app.any_buffer_dirty(), "but beta is dirty");

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());
    app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges)),
        "non-active dirty must still guard the switch"
    );
}

/// A REFUSED save during a multi-dirty workspace switch aborts the switch
/// and keeps every buffer (mirrors the single-buffer save-failure behaviour).
#[test]
fn workspace_switch_s_refused_save_aborts_switch() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = two_endpoint_app(dir_a.path());
    app.history = Some(HistoryStore::in_memory().unwrap());
    let alpha_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
        .unwrap();
    let beta_idx = app
        .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
        .unwrap();
    // beta: a saveable edit; alpha: an UNSAVEABLE literal-secret auth.
    app.active = beta_idx;
    *app.test_editor() = EditorState::new(Lines::from("beta-body"));
    app.active = alpha_idx;
    app.selected_mut().unwrap().endpoint.request.auth = Some(churl_core::model::Auth::Bearer {
        token: "ghp_literal_secret".to_owned(),
    });
    assert!(app.any_buffer_dirty());

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());
    app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
        .unwrap();
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges)
    ));

    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .unwrap();
    // Switch aborted: still on workspace "demo", both buffers kept.
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(
        app.workspace.as_ref().unwrap().manifest().name,
        "demo",
        "refused save must abort the switch"
    );
    assert_eq!(app.buffers.len(), 2, "no buffer destroyed");
    assert!(app.any_buffer_dirty(), "the refused buffer stays dirty");
}

// ---- concurrent-load runner ----

/// Builds an app with one selected endpoint (targeting `url`) and opens the
/// load runner over it. Client is `None`, so a run resets rows to Pending and
/// leaves them (runtime-free); the state machine is exercised by injecting
/// `on_load_started`/`on_load_result`.
fn load_app(root: &Path, url: &str) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("ping.toml"),
        format!("seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"{url}\"\n"),
    )
    .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.guarded_load(PendingLoad::File(coll.join("ping.toml")))
        .unwrap();
    app.open_load_runner();
    app
}

fn load_gen(app: &App) -> u64 {
    app.load_runner().unwrap().run_generation
}

fn load_status(app: &App, i: usize) -> LoadStatus {
    app.load_runner().unwrap().results[i].status.clone()
}

#[test]
fn load_runner_opens_with_resolved_request() {
    let dir = tempfile::tempdir().unwrap();
    let app = load_app(dir.path(), "https://api.test/ping");
    assert!(matches!(app.mode, Mode::LoadRunner(_)));
    let runner = app.load_runner().unwrap();
    assert_eq!(runner.url, "https://api.test/ping");
    assert_eq!(runner.endpoint_path.as_deref(), Some("api/ping.toml"));
    assert!(!runner.running, "must not auto-run");
    assert!(runner.results.is_empty(), "no rows until a run starts");
    assert!(app.load_request.is_some(), "request resolved once at open");
}

// ---- `<leader>l f` pick-then-load-test: dirty-safe + no flag leak ----

/// A two-endpoint workspace with `alpha` loaded into the request pane.
fn load_pick_app(root: &Path) -> App {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    for name in ["alpha", "beta"] {
        std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
    }
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.guarded_load(PendingLoad::File(coll.join("alpha.toml")))
        .unwrap();
    app
}

/// Drives the search picker to the single item matching `query` and accepts it.
fn pick_search(app: &mut App, query: &str) {
    for c in query.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    app.accept_overlay().unwrap();
}

/// STAGE 2: `<leader>l f` with a DIRTY editor + a DIFFERENT picked endpoint no
/// longer defers — each endpoint has its own buffer, so beta opens as a NEW
/// buffer (dirty alpha stays open) and the runner opens over beta.
#[test]
fn load_runner_pick_dirty_opens_new_buffer_and_runner() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_pick_app(dir.path());
    // Make the loaded `alpha` dirty.
    *app.test_editor() = EditorState::new(Lines::from("dirty body"));
    assert!(app.is_dirty());
    let alpha_file = app.selected().unwrap().file.clone();

    app.open_load_runner_pick().unwrap();
    assert!(matches!(app.mode, Mode::Search));
    assert!(app.picker_after_pick(), "intent armed");
    // Pick the DIFFERENT endpoint (beta).
    pick_search(&mut app, "beta");

    // No confirm — the runner opens over the freshly-focused beta buffer.
    assert!(matches!(app.mode, Mode::LoadRunner(_)));
    assert_eq!(app.load_runner().unwrap().url, "https://api.test/beta");
    assert_eq!(app.buffers.len(), 2, "beta pushed as a new buffer");
    assert_eq!(app.selected().unwrap().display_path, "api/beta");
    let alpha_idx = app.buffer_index_for_path(&alpha_file).unwrap();
    assert!(
        app.buffers[alpha_idx].is_dirty(),
        "alpha's unsaved edits are preserved in its own buffer"
    );
    assert!(!app.picker_after_pick());
}

/// A clean `<leader>l f` pick loads the endpoint AND opens the load runner.
#[test]
fn load_runner_pick_clean_loads_and_opens_runner() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_pick_app(dir.path());
    assert!(!app.is_dirty());
    app.open_load_runner_pick().unwrap();
    pick_search(&mut app, "beta");
    assert!(
        matches!(app.mode, Mode::LoadRunner(_)),
        "clean pick opens the runner"
    );
    // The runner targets the newly-picked endpoint, not the previously loaded one.
    assert_eq!(app.load_runner().unwrap().url, "https://api.test/beta");
    assert!(!app.picker_after_pick());
}

/// BLOCKER 3a: Esc-cancelling the `<leader>l f` picker clears the one-shot flag
/// so the NEXT plain `<leader>f` / `/` search does not spuriously open the runner.
#[test]
fn load_runner_pick_esc_clears_flag() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_pick_app(dir.path());
    app.open_load_runner_pick().unwrap();
    assert!(app.picker_after_pick());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        !app.picker_after_pick(),
        "esc-cancel must clear the one-shot intent"
    );
}

/// BLOCKER 3b: an empty-result Enter (nothing selected) clears the flag too.
#[test]
fn load_runner_pick_empty_enter_clears_flag() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_pick_app(dir.path());
    app.open_load_runner_pick().unwrap();
    // Type a query that matches nothing → the picker has no current item.
    for c in "zzzznomatch".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    assert!(
        app.picker_state()
            .and_then(picker::PickerState::current)
            .is_none(),
        "no current item for a non-matching query"
    );
    app.accept_overlay().unwrap();
    assert!(
        !app.picker_after_pick(),
        "empty-result Enter must clear the one-shot intent"
    );
    assert!(app.load_runner().is_none());
}

#[test]
fn load_run_injected_results_update_stats_and_finish() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.history = Some(HistoryStore::in_memory().unwrap());
    // total=3, then start (client None → 3 pending rows, running).
    app.load_runner_mut().unwrap().cfg.total = 3;
    app.start_load_run();
    let g = load_gen(&app);
    assert!(app.load_runner().unwrap().running);
    assert_eq!(load_status(&app, 0), LoadStatus::Pending);

    app.on_load_started(g, 0);
    assert_eq!(load_status(&app, 0), LoadStatus::Running);
    app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
    app.on_load_result(g, 1, Ok(ok_resp(500, "b")));
    assert!(!app.load_runner().unwrap().finished);
    app.on_load_result(g, 2, Err("connection refused".to_owned()));

    let runner = app.load_runner().unwrap();
    assert!(runner.finished && !runner.running && !runner.cancelled);
    assert_eq!(runner.completed, 3);
    assert_eq!(runner.stats.ok, 1);
    assert_eq!(runner.stats.failed, 1);
    assert_eq!(runner.stats.errored, 1);
    assert_eq!(load_status(&app, 0), LoadStatus::Ok(200));
    assert_eq!(load_status(&app, 1), LoadStatus::Failed(500));
    assert!(matches!(load_status(&app, 2), LoadStatus::Error(_)));

    // Exactly one batch summary row, none in per-endpoint history.
    let batches = app
        .history
        .as_ref()
        .unwrap()
        .recent_load_batches(10)
        .unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].summary.total, 3);
    assert_eq!(batches[0].summary.ok_count, 1);
    assert!(!batches[0].summary.cancelled);
    // mean latency is persisted (ok_resp timings are 5ms; error has none).
    assert_eq!(batches[0].summary.mean_ms, Some(5));
    assert_eq!(app.history.as_ref().unwrap().recent(10).unwrap().len(), 0);
}

// ---- unified runner response viewer (parity + preserved nav) ----

/// A JSON response carrying a `content-type` header so the viewer defaults to
/// pretty and JSON folding/sort are available (mirrors a real server response).
fn json_resp(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![Header {
            name: "content-type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    }
}

/// Opens the load runner over `ping`, runs one copy that lands a JSON `Done`
/// response, and focuses the Response region — the setup every parity test
/// shares. `body` is the raw wire body.
fn load_app_with_response(root: &Path, body: &str) -> App {
    let mut app = load_app(root, "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 1;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Ok(json_resp(body)));
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Response;
    app
}

/// The selected load-runner row's live `ResponseView`, for white-box asserts.
fn load_view(app: &App) -> &ResponseView {
    match app.load_runner().unwrap().selected_response().unwrap() {
        ResponseState::Done { view } => view,
        other => panic!("selected row is not Done: {other:?}"),
    }
}

fn norm(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn keyc(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Drives one full-UI render through a `TestBackend` so the runner response
/// geometry (total_rows / viewport height+width) is measured — the cursor-nav
/// and fold-at-cursor tests need a real geometry, not the zero default.
fn render_once(app: &mut App) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(116, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| render(frame, app)).expect("draw");
}

/// The active response surface resolves to the runner ONLY when its Response
/// region is focused — so Config/Results focus never routes response actions.
#[test]
fn load_response_surface_gated_on_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");
    assert_eq!(app.active_response_surface(), ResponseSurface::LoadRunner);
    // Refocus config: a response action must now be a no-op on the runner view.
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::ConfigHeader;
    assert_eq!(app.active_response_surface(), ResponseSurface::Main);
    let before = load_view(&app).pretty();
    app.handle_key(norm('p')).unwrap();
    assert_eq!(
        load_view(&app).pretty(),
        before,
        "p on Config focus must not touch the runner view"
    );
}

/// `p`/`s`/`#`/`W`/`h` toggle the SELECTED row's `ResponseView` exactly as the
/// main pane does — proving one shared path, not a re-plumbed subset.
#[test]
fn load_response_parity_toggles() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");

    // p: pretty on by default for JSON → toggles to raw.
    assert!(load_view(&app).pretty());
    app.handle_key(norm('p')).unwrap();
    assert!(!load_view(&app).pretty(), "p toggled pretty off");
    app.handle_key(norm('p')).unwrap();
    assert!(load_view(&app).pretty(), "p toggled pretty back on");

    // s: A→Z key sort on the pretty JSON body.
    assert!(!load_view(&app).sort_keys());
    app.handle_key(norm('s')).unwrap();
    assert!(load_view(&app).sort_keys(), "s toggled sort on");

    // #: line-number gutter (default on) toggles off.
    assert!(load_view(&app).line_numbers());
    app.handle_key(norm('#')).unwrap();
    assert!(!load_view(&app).line_numbers(), "# toggled gutter off");

    // W: wrap.
    assert!(!load_view(&app).wrap());
    app.handle_key(shift('W')).unwrap();
    assert!(load_view(&app).wrap(), "W toggled wrap on");

    // h: headers view.
    assert_eq!(load_view(&app).view_mode(), ViewMode::Body);
    app.handle_key(norm('h')).unwrap();
    assert_eq!(
        load_view(&app).view_mode(),
        ViewMode::Headers,
        "h switched to headers"
    );
}

/// `o` folds the region at the RUNNER cursor (not the main pane's), and `O`
/// folds all — proving fold-at-cursor uses the mode-aware geometry.
#[test]
fn load_response_fold_uses_runner_cursor() {
    let dir = tempfile::tempdir().unwrap();
    // A nested object so there is a foldable region.
    let mut app = load_app_with_response(dir.path(), "{\"outer\":{\"x\":1,\"y\":2}}");
    // Render once so the runner geometry (total_rows/viewport) is measured.
    render_once(&mut app);
    let rows_before = load_view(&app).total_display_rows(40);
    // Cursor at the opener line (row 0 is `{`, row 1 is `"outer": {`).
    app.load_runner_mut().unwrap().geometry.cursor = 1;
    app.handle_key(norm('o')).unwrap();
    let rows_after = load_view(&app).total_display_rows(40);
    assert!(
        rows_after < rows_before,
        "fold at the runner cursor collapsed rows ({rows_before} → {rows_after})"
    );
}

/// `J`/`K` jump the runner Response cursor between collapsible JSON nodes,
/// proving structural navigation routes through the runner surface.
#[test]
fn load_response_structural_jump_moves_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"outer\":{\"x\":1,\"y\":2}}");
    render_once(&mut app);
    // Cursor starts on the root opener (row 0). `J` jumps to the first
    // collapsible node — the `"outer": {` opener at row 1.
    assert_eq!(app.load_runner().unwrap().geometry.cursor, 0);
    app.handle_key(shift('J')).unwrap();
    assert_eq!(
        app.load_runner().unwrap().geometry.cursor,
        1,
        "J moved the cursor to the outer opener"
    );
    // `K` climbs back out to the root opener.
    app.handle_key(shift('K')).unwrap();
    assert_eq!(
        app.load_runner().unwrap().geometry.cursor,
        0,
        "K returned to the root opener"
    );
}

/// `J` on a non-JSON body notifies (mirroring the fold guard) instead of moving
/// the cursor — structural navigation is JSON-only.
#[tokio::test]
async fn structural_jump_non_json_notifies() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    open_bare_endpoint(&mut app);
    app.generation = 5;
    set_active_in_flight(&mut app, 5);
    // `response()` carries no content-type, so the body is not JSON.
    app.on_response(5, Ok(response()), meta());
    app.focus = Pane::Response;
    render_once(&mut app);
    app.handle_key(shift('J')).unwrap();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("folding: JSON responses only"),
        "J on a non-JSON body notifies rather than moving"
    );
    assert_eq!(
        app.response_cursor_logical(),
        Some(0),
        "the cursor did not move"
    );
}

/// `/` opens body search over the RUNNER response, `n` steps matches, and Esc
/// returns to the runner (NOT to Normal mode — the runner stays open).
#[test]
fn load_response_search_targets_runner_and_returns() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"needle\":\"needle\"}");
    render_once(&mut app);
    app.handle_key(norm('/')).unwrap();
    assert!(matches!(app.mode, Mode::BodySearch));
    assert!(
        matches!(app.body_search_return, Mode::LoadRunner(_)),
        "search remembers to return to the runner"
    );
    for c in "needle".chars() {
        app.handle_key(norm(c)).unwrap();
    }
    assert!(
        load_view(&app).search().is_some_and(|s| s.count() >= 2),
        "search found matches in the runner view"
    );
    app.handle_key(keyc(KeyCode::Esc)).unwrap();
    assert!(
        matches!(app.mode, Mode::LoadRunner(_)),
        "Esc returns to the runner"
    );
}

/// Regression lock: a load result that lands WHILE body-search is open
/// over the runner must reach the PARKED runner state (moved into
/// `body_search_return` when `/` opened `Mode::BodySearch`), not fall on the
/// floor because `self.mode` is momentarily `Mode::BodySearch`. Proves
/// `load_runner_mut()` resolves the parked state, the batch finalizes mid-search
/// (finished + summary written), and Esc restores `Mode::LoadRunner(state)` with
/// the recorded result intact. Guards the fold against PRs 2–3 touching this area.
#[test]
fn load_result_lands_on_parked_runner_during_body_search() {
    let dir = tempfile::tempdir().unwrap();
    // Two-copy batch so landing copy #1 finalizes the run (finished + summary),
    // exercising every `load_runner()`/`load_runner_mut()` seam on the FINISH path
    // while the runner is parked behind body-search.
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 2;
    app.start_load_run();
    let g = load_gen(&app);
    // Copy #0 lands normally (runner still live in `Mode::LoadRunner`); focus its
    // Response region so `/` targets the runner surface.
    app.on_load_result(g, 0, Ok(json_resp("{\"needle\":\"needle\"}")));
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Response;
    render_once(&mut app);

    // Open body-search over the runner → mode flips to BodySearch, runner parked.
    app.handle_key(norm('/')).unwrap();
    assert!(matches!(app.mode, Mode::BodySearch));
    assert!(
        matches!(app.body_search_return, Mode::LoadRunner(_)),
        "the runner state is parked in body_search_return, not dropped"
    );
    // The parked runner is still reachable through the accessor mid-search.
    assert!(
        app.load_runner().is_some(),
        "load_runner() resolves the parked runner while Mode::BodySearch"
    );
    assert!(
        !app.load_runner().unwrap().finished,
        "batch not finished yet — copy #1 still pending"
    );

    // Copy #1 lands WHILE body-search is active. It must reach the parked runner.
    app.on_load_result(g, 1, Ok(json_resp("{\"ok\":true}")));

    // The result reached the parked runner: row #1 recorded and the batch
    // finalized (finished flips true, written on the finish path via load_runner()).
    assert!(matches!(app.mode, Mode::BodySearch), "still searching");
    assert_eq!(
        load_status(&app, 1),
        LoadStatus::Ok(200),
        "copy #1 recorded on the PARKED runner mid-search"
    );
    assert!(
        app.load_runner().unwrap().finished,
        "batch finished on the parked runner (last copy landed)"
    );

    // Esc out of body-search → the runner mode is restored intact, with BOTH
    // results still present (row #0 from before the search, row #1 landed during).
    app.handle_key(keyc(KeyCode::Esc)).unwrap();
    assert!(
        matches!(app.mode, Mode::LoadRunner(_)),
        "Esc restores Mode::LoadRunner"
    );
    assert_eq!(load_status(&app, 0), LoadStatus::Ok(200), "row #0 intact");
    assert_eq!(
        load_status(&app, 1),
        LoadStatus::Ok(200),
        "row #1 (landed mid-search) survived the restore"
    );
    assert!(
        app.load_runner().unwrap().finished,
        "finished state survived the restore"
    );
}

/// Horizontal pan (`L`) pans the runner view's `h_scroll`, and copy (`y`/`Y`)
/// returns the BYTE-EXACT raw wire bytes (byte-exactness invariant holds in the runner).
#[test]
fn load_response_hscroll_and_byte_exact_copy() {
    let dir = tempfile::tempdir().unwrap();
    // A raw body with a tab + control byte: pretty-off so copy sees wire bytes.
    let raw = "{\"k\":\"a\tb\"}";
    let mut app = load_app_with_response(dir.path(), raw);
    // Turn pretty off so the displayed text is the sanitized raw (tab expanded)
    // while copy must still return the exact tab byte.
    app.handle_key(norm('p')).unwrap();
    assert!(!load_view(&app).pretty());

    // L pans the horizontal window right (wrap is off).
    assert_eq!(load_view(&app).h_scroll(), 0);
    app.handle_key(shift('L')).unwrap();
    assert!(load_view(&app).h_scroll() > 0, "L panned h_scroll right");

    // y copies the full raw body byte-exact (the tab survives).
    app.handle_key(norm('y')).unwrap();
    let copied = app.pending_clipboard.as_ref().unwrap().payload.clone();
    assert_eq!(
        copied, raw,
        "y returns byte-exact raw wire bytes (tab intact)"
    );
}

// ---- drive-test #4a: honest, yankable failed-response row ----

/// `y` on a load-runner `Failed` row copies useful text (the error + URL)
/// via the shared copy handler — NOT a silent no-op. The failed row carries
/// no `ResponseView`, so this exercises the state-branch fallback.
#[test]
fn load_failed_row_y_copies_error_not_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 1;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Err("connection refused".to_owned()));
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Response;
    // Sanity: the selected row is Failed (no view to copy).
    assert!(matches!(
        app.active_response(),
        ResponseState::Failed { .. }
    ));

    app.handle_key(norm('y')).unwrap();
    let copied = app
        .pending_clipboard
        .as_ref()
        .expect("y on a Failed row must enqueue a copy (not a no-op)")
        .payload
        .clone();
    assert!(
        copied.contains("connection refused"),
        "copied text carries the error: {copied:?}"
    );
    assert!(
        copied.contains("https://api.test/ping"),
        "copied text carries the request URL: {copied:?}"
    );
}

/// Drive-test #4a fold-in: `y` on a `Dropped` (memory-evicted) load row has
/// no body to copy, but must NOT be a silent no-op — it sets a status message
/// explaining the body was not retained, and enqueues nothing.
#[test]
fn load_dropped_row_y_sets_message_not_silent_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 1;
    app.start_load_run();
    // Force the selected row into the memory-evicted Dropped state.
    {
        let runner = app.load_runner_mut().unwrap();
        runner.results[0].response = ResponseState::Dropped {
            status: 200,
            timing: None,
            size: 1234,
        };
        runner.focus = load_runner::RunnerFocus::Response;
    }
    assert!(matches!(
        app.active_response(),
        ResponseState::Dropped { .. }
    ));

    app.handle_key(norm('y')).unwrap();
    assert!(
        app.pending_clipboard.is_none(),
        "a Dropped row has nothing to copy — nothing is enqueued"
    );
    let msg = app
        .message
        .as_ref()
        .map(|m| m.text.as_str())
        .expect("y on a Dropped row must set a status message (not a no-op)");
    assert!(
        msg.contains("not retained"),
        "message explains the body was evicted: {msg:?}"
    );
    assert!(!msg.is_empty());
}

/// `Y` (copy-line) on a `Dropped`/`Failed`/`Idle` row — no view, no line —
/// also gives feedback instead of a silent no-op.
#[test]
fn load_dropped_row_shift_y_sets_message_not_silent_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 1;
    app.start_load_run();
    {
        let runner = app.load_runner_mut().unwrap();
        runner.results[0].response = ResponseState::Dropped {
            status: 200,
            timing: None,
            size: 1234,
        };
        runner.focus = load_runner::RunnerFocus::Response;
    }
    app.response_copy_line();
    assert!(
        app.pending_clipboard.is_none(),
        "no line to copy — nothing enqueued"
    );
    let msg = app.message.as_ref().map(|m| m.text.as_str()).unwrap_or("");
    assert!(
        msg.contains("not retained"),
        "Y on a Dropped row reports why: {msg:?}"
    );
}

/// `Y` on a `Failed` row (no view) gives the generic feedback — still not a
/// silent no-op (drive-test #4a fold-in: `Y` on Failed was silent post-4a).
#[test]
fn load_failed_row_shift_y_sets_message_not_silent_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 1;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Err("connection refused".to_owned()));
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Response;
    assert!(matches!(
        app.active_response(),
        ResponseState::Failed { .. }
    ));
    app.response_copy_line();
    assert!(
        app.pending_clipboard.is_none(),
        "Y copies a line, not the failure blurb — nothing enqueued"
    );
    let msg = app.message.as_ref().map(|m| m.text.as_str()).unwrap_or("");
    assert_eq!(msg, "nothing to copy", "generic feedback, not silence");
}

/// `y` on a sequence-runner `Failed` step copies the error via the SAME
/// shared handler — full parity with the load runner and main pane.
#[test]
fn sequence_failed_step_y_copies_error_not_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = sequence_app(dir.path(), "continue", "");
    let g = runner_gen(&app);
    app.on_sequence_step(g, 0, Err("dns error".to_owned()));
    let runner = app.sequence_runner_mut().unwrap();
    runner.selected = 0;
    runner.focus = sequence_runner::RunnerFocus::Response;
    assert!(matches!(
        app.active_response(),
        ResponseState::Failed { .. }
    ));

    app.handle_key(norm('y')).unwrap();
    let copied = app
        .pending_clipboard
        .as_ref()
        .expect("y on a Failed step must enqueue a copy (not a no-op)")
        .payload
        .clone();
    assert!(
        copied.contains("dns error"),
        "copied text carries the error: {copied:?}"
    );
}

/// `y` on a MAIN-pane `Failed` response copies the error + method + URL
/// through the same shared handler (the third unified viewer).
#[test]
fn main_failed_response_y_copies_error_and_request() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.buffers = vec![Buffer::endpoint(selected_with("a.toml", None))];
    app.active = 0;
    app.buffers[0].as_endpoint_mut().unwrap().response = ResponseState::Failed {
        error: "connection refused".to_owned(),
        meta: meta(),
    };
    app.set_focus(Pane::Response);
    app.response_copy_view();
    let copied = app
        .pending_clipboard
        .as_ref()
        .expect("y on a Failed main-pane response must enqueue a copy")
        .payload
        .clone();
    assert!(
        copied.contains("connection refused"),
        "carries the error: {copied:?}"
    );
    assert!(
        copied.contains("GET") && copied.contains("https://api.test/x"),
        "carries the request method+URL from meta: {copied:?}"
    );
}

/// Unit: `failure_copy_text` yields the request line + error for `Failed`,
/// and `None` for every other state (so the copy handler falls through only
/// on a failure). Never fabricates a status/body/timing.
#[test]
fn failure_copy_text_shape() {
    let failed = ResponseState::Failed {
        error: "boom".to_owned(),
        meta: meta(),
    };
    let text = failed.failure_copy_text().expect("Failed yields text");
    assert_eq!(text, "GET https://api.test/x\nerror: boom");

    // Non-Failed states have no failure text.
    assert!(ResponseState::Idle.failure_copy_text().is_none());
    assert!(ResponseState::Cancelled.failure_copy_text().is_none());
    assert!(
        ResponseState::Dropped {
            status: 200,
            timing: None,
            size: 0,
        }
        .failure_copy_text()
        .is_none(),
        "Dropped has no body to copy — stays a no-op"
    );

    // A runner meta with an empty method omits it rather than padding.
    let url_only = ResponseState::Failed {
        error: "dns".to_owned(),
        meta: load_result_meta("https://api.test/ping"),
    };
    assert_eq!(
        url_only.failure_copy_text().unwrap(),
        "https://api.test/ping\nerror: dns"
    );
}

/// `j`/`k`/`g`/`G`/`Ctrl-D` move the runner Response cursor through the SAME
/// path the main pane uses (mode-aware geometry), not a local table.
#[test]
fn load_response_cursor_nav_shared_path() {
    let dir = tempfile::tempdir().unwrap();
    let body = (0..30)
        .map(|i| format!("\"k{i}\":{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut app = load_app_with_response(dir.path(), &format!("{{{body}}}"));
    render_once(&mut app);
    assert_eq!(app.load_runner().unwrap().geometry.cursor, 0);
    app.handle_key(norm('j')).unwrap();
    assert_eq!(
        app.load_runner().unwrap().geometry.cursor,
        1,
        "j moved the runner cursor down via the shared path"
    );
    app.handle_key(shift('G')).unwrap();
    let bottom = app.load_runner().unwrap().geometry.cursor;
    assert!(bottom > 1, "G jumped to the bottom row");
    app.handle_key(norm('k')).unwrap();
    assert_eq!(
        app.load_runner().unwrap().geometry.cursor,
        bottom - 1,
        "k moved up one"
    );
}

/// PRESERVED NAV: from Response focus, Tab still cycles regions and Ctrl-R
/// still re-runs — a response action never eats these runner-owned keys.
#[test]
fn load_runner_nav_not_shadowed_by_response() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"a\":1}");
    assert_eq!(
        app.load_runner().unwrap().focus,
        load_runner::RunnerFocus::Response
    );
    // Tab cycles Response → ConfigHeader (runner-owned; not a response action).
    app.handle_key(keyc(KeyCode::Tab)).unwrap();
    assert_eq!(
        app.load_runner().unwrap().focus,
        load_runner::RunnerFocus::ConfigHeader,
        "Tab still cycles regions"
    );
    // Back to Response; Ctrl-R still (re-)runs the batch (bumps generation).
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Response;
    let g0 = load_gen(&app);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(
        load_gen(&app) > g0,
        "Ctrl-R still re-runs from Response focus"
    );
}

/// PRESERVED NAV: `q` from Response focus still closes the runner when idle
/// (a response action never eats it).
#[test]
fn load_runner_q_closes_from_response_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app_with_response(dir.path(), "{\"a\":1}");
    // The one-copy batch has finished (not running), so q closes immediately.
    assert!(!app.load_runner().unwrap().is_running());
    assert_eq!(
        app.load_runner().unwrap().focus,
        load_runner::RunnerFocus::Response
    );
    app.handle_key(norm('q')).unwrap();
    assert!(app.load_runner().is_none(), "q still closes the runner");
    assert!(matches!(app.mode, Mode::Normal));
}

/// PRESERVED NAV: when Results-focused, `j`/`k` still select rows (the response
/// nav must not shadow the results list).
#[test]
fn load_results_selection_not_shadowed() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 3;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Ok(json_resp("{\"a\":1}")));
    app.on_load_result(g, 1, Ok(json_resp("{\"b\":2}")));
    app.on_load_result(g, 2, Ok(json_resp("{\"c\":3}")));
    app.load_runner_mut().unwrap().focus = load_runner::RunnerFocus::Results;
    app.load_runner_mut().unwrap().selected = 0;
    app.handle_key(norm('j')).unwrap();
    assert_eq!(
        app.load_runner().unwrap().selected,
        1,
        "j selects the next results row (not response scroll)"
    );
}

#[test]
fn load_stale_result_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 2;
    app.start_load_run();
    let g = load_gen(&app);
    // A result from a superseded generation must not land.
    app.on_load_result(g + 99, 0, Ok(ok_resp(200, "x")));
    assert_eq!(load_status(&app, 0), LoadStatus::Pending);
    assert_eq!(app.load_runner().unwrap().completed, 0);
}

#[test]
fn load_cancel_marks_pending_and_writes_partial_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.load_runner_mut().unwrap().cfg.total = 4;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
    app.cancel_load_run();

    let runner = app.load_runner().unwrap();
    assert!(runner.finished && runner.cancelled && !runner.running);
    assert_eq!(load_status(&app, 0), LoadStatus::Ok(200));
    assert!(
        runner.results[1..]
            .iter()
            .all(|r| r.status == LoadStatus::Cancelled),
        "every non-terminal row is cancelled"
    );
    // The generation was bumped so a straggler result is dropped.
    app.on_load_result(g, 1, Ok(ok_resp(200, "late")));
    assert_eq!(load_status(&app, 1), LoadStatus::Cancelled);

    let batches = app
        .history
        .as_ref()
        .unwrap()
        .recent_load_batches(10)
        .unwrap();
    assert_eq!(batches.len(), 1);
    assert!(
        batches[0].summary.cancelled,
        "partial summary marked cancelled"
    );
    assert_eq!(batches[0].summary.ok_count, 1);
}

/// A launched-then-cancelled row carries a real time-to-cancel (read out
/// of `InFlight { started }`), while a never-launched pending row keeps
/// `timing = None` — no fabricated zero.
#[test]
fn load_cancel_records_time_to_cancel_for_launched_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.load_runner_mut().unwrap().cfg.total = 3;
    app.start_load_run();
    let g = load_gen(&app);
    // Mark rows 0 and 1 launched (InFlight); leave row 2 never-launched.
    app.on_load_started(g, 0);
    app.on_load_started(g, 1);
    app.cancel_load_run();

    let runner = app.load_runner().unwrap();
    assert_eq!(runner.results[0].status, LoadStatus::Cancelled);
    assert!(
        runner.results[0].timing.is_some(),
        "launched-then-cancelled row shows a time-to-cancel"
    );
    assert!(
        runner.results[1].timing.is_some(),
        "second launched row also times"
    );
    assert_eq!(runner.results[2].status, LoadStatus::Cancelled);
    assert!(
        runner.results[2].timing.is_none(),
        "never-launched pending row keeps timing None (no fabricated zero)"
    );
}

#[test]
fn load_rerun_while_running_writes_cancelled_summary_and_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.load_runner_mut().unwrap().cfg.total = 4;
    app.start_load_run();
    let g1 = load_gen(&app);
    app.on_load_result(g1, 0, Ok(ok_resp(200, "a")));
    assert!(app.load_runner().unwrap().running);

    // Ctrl-R while running: cancel-record the partial, then restart fresh
    // (Run is Ctrl-R as of the 2026-07-10 owner decision).
    app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .unwrap();
    let runner = app.load_runner().unwrap();
    assert!(runner.running, "a fresh run started");
    assert!(!runner.finished && !runner.cancelled);
    assert!(runner.run_generation > g1, "fresh run on a new generation");
    assert_eq!(runner.completed, 0, "fresh rows");
    assert!(
        runner
            .results
            .iter()
            .all(|r| r.status == LoadStatus::Pending),
        "every row reset to pending"
    );

    // The interrupted run left exactly one cancelled summary capturing what
    // completed so far — the partial is not lost.
    let batches = app
        .history
        .as_ref()
        .unwrap()
        .recent_load_batches(10)
        .unwrap();
    assert_eq!(
        batches.len(),
        1,
        "the interrupted run's partial was recorded"
    );
    assert!(batches[0].summary.cancelled);
    assert_eq!(batches[0].summary.ok_count, 1);
}

#[test]
fn load_close_while_running_writes_cancelled_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.load_runner_mut().unwrap().cfg.total = 4;
    app.start_load_run();
    let g = load_gen(&app);
    app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
    app.on_load_result(g, 1, Ok(ok_resp(500, "b")));

    // Close mid-run: `q` asks to confirm, `y` closes.
    app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    assert!(app.load_runner().unwrap().confirming_close);
    app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .unwrap();
    assert!(app.load_runner().is_none(), "runner closed");
    assert!(matches!(app.mode, Mode::Normal));

    // Closing mid-run recorded the partial (cancelled) summary.
    let batches = app
        .history
        .as_ref()
        .unwrap()
        .recent_load_batches(10)
        .unwrap();
    assert_eq!(batches.len(), 1, "close mid-run recorded the partial");
    assert!(batches[0].summary.cancelled);
    assert_eq!(batches[0].summary.ok_count, 1);
    assert_eq!(batches[0].summary.fail_count, 1);
}

#[test]
fn load_guardrail_refuse_blocks_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    // Above the hard cap (default max_total = 10_000).
    app.load_runner_mut().unwrap().cfg.total = 20_000;
    app.request_load_run();
    let runner = app.load_runner().unwrap();
    assert!(!runner.running, "a refused run never starts");
    assert!(runner.pending_confirm.is_none(), "refuse does not prompt");
    assert!(app.message.is_some(), "refusal is surfaced loudly");
}

#[test]
fn load_guardrail_warn_requires_confirm_then_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), "https://api.test/ping");
    // Above warn_total (default 100), below the hard cap.
    app.load_runner_mut().unwrap().cfg.total = 500;
    app.request_load_run();
    let runner = app.load_runner().unwrap();
    assert!(!runner.running, "warn does not run immediately");
    let confirm = runner.pending_confirm.clone().expect("confirm shown");
    assert!(
        confirm.contains("500"),
        "confirm names the count: {confirm}"
    );
    assert!(
        confirm.contains("https://api.test/ping"),
        "confirm shows the target URL: {confirm}"
    );
    // Accepting the confirm (`y`) starts the run.
    app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .unwrap();
    let runner = app.load_runner().unwrap();
    assert!(runner.pending_confirm.is_none());
    assert!(runner.running, "confirmed run started");
    assert_eq!(runner.results.len(), 500);
}

/// A request-counting responder that holds each request open, so a cancel
/// mid-batch is observable as "the server never saw the un-launched copies".
struct CountingSlowResponder {
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    delay: Duration,
}

impl wiremock::Respond for CountingSlowResponder {
    fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        wiremock::ResponseTemplate::new(200).set_delay(self.delay)
    }
}

/// The batch-cancel non-negotiable, proven live: the runner owns a SINGLE
/// launcher task; cancelling aborts it, which drops the `buffer_unordered`
/// fan-out and every in-flight reqwest future — so the un-launched copies of a
/// large batch are never fired. With concurrency 2 and a slow server, a cancel
/// right after launch means the server sees only a handful of the copies, not
/// all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_cancel_aborts_the_batch_live() {
    use churl_core::http::{DEFAULT_TIMEOUT, build_client};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(CountingSlowResponder {
            count: Arc::clone(&count),
            delay: Duration::from_millis(400),
        })
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let mut app = load_app(dir.path(), &format!("{}/slow", server.uri()));
    app.client = Some(build_client(DEFAULT_TIMEOUT).unwrap());
    app.history = Some(HistoryStore::in_memory().unwrap());
    {
        let cfg = &mut app.load_runner_mut().unwrap().cfg;
        cfg.total = 20;
        cfg.concurrency = 2;
        cfg.interval = Duration::ZERO;
    }
    app.start_load_run();
    assert!(app.load_abort.is_some(), "a launcher task is running");

    // Let the first couple of copies actually reach the server, then cancel.
    tokio::time::sleep(Duration::from_millis(120)).await;
    app.cancel_load_run();

    // Give any leaked / detached work ample time to hit the server.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let seen = count.load(Ordering::SeqCst);
    assert!(
        seen < 20,
        "cancel must abort the batch: the server saw {seen}/20 copies (all fired → cancel leaked)"
    );
    assert!(
        app.load_runner().unwrap().cancelled,
        "runner marked cancelled"
    );
}

// ---- Buffer refactor (Stage 1) unit tests ----

/// Builds a `SelectedEndpoint` with the given file + body for buffer tests.
fn selected_with(file: &str, body: Option<&str>) -> SelectedEndpoint {
    SelectedEndpoint {
        display_path: format!("coll/{file}"),
        file: std::path::PathBuf::from(file),
        collection: 0,
        endpoint: Endpoint {
            seq: 0,
            name: "ep".to_owned(),
            assertions: Vec::new(),
            request: Request {
                method: churl_core::model::Method::Get,
                url: "https://api.test/x".to_owned(),
                headers: Vec::new(),
                params: Vec::new(),
                body: body.map(|c| Body {
                    kind: BodyKind::Text,
                    content: c.to_owned(),
                }),
                auth: None,
                insecure: false,
            },
        },
    }
}

/// `open_or_focus_buffer` builds a buffer whose editor is seeded from the
/// request body and whose `loaded_snapshot` clones the pristine endpoint.
/// Stage 2: a fresh open seeds the editor/snapshot; a SECOND distinct
/// endpoint pushes a new buffer (never replaces).
#[test]
fn open_or_focus_buffer_seeds_editor_and_snapshot() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("hello body")));
    assert_eq!(app.buffers.len(), 1);
    assert_eq!(app.active, 0);
    let b = app.active_endpoint_buffer().unwrap();
    assert_eq!(String::from(b.editor.lines.clone()), "hello body");
    assert_eq!(
        b.loaded_snapshot.request.body.as_ref().unwrap().content,
        "hello body"
    );
    assert!(!app.is_dirty(), "freshly opened buffer is not dirty");

    // Opening a second distinct endpoint PUSHES a new buffer + focuses it.
    app.open_or_focus_buffer(selected_with("b.toml", None));
    assert_eq!(app.buffers.len(), 2, "distinct endpoint pushes a buffer");
    assert_eq!(app.active, 1);
    assert_eq!(
        app.active_endpoint_buffer().unwrap().endpoint.file,
        PathBuf::from("b.toml")
    );
}

/// Stage 2: re-opening an already-open endpoint DEDUPS — focuses the existing
/// buffer instead of pushing a duplicate, preserving its in-memory edits.
#[test]
fn open_or_focus_buffer_dedups_and_focuses_existing() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("body a")));
    app.open_or_focus_buffer(selected_with("b.toml", Some("body b")));
    assert_eq!(app.buffers.len(), 2);
    assert_eq!(app.active, 1);
    // Edit B so we can prove the dedup keeps its edits.
    *app.test_editor() = EditorState::new(Lines::from("edited b"));
    assert!(app.is_dirty());

    // Re-open A: focuses it, no new buffer.
    app.open_or_focus_buffer(selected_with("a.toml", Some("body a")));
    assert_eq!(app.buffers.len(), 1 + 1, "no duplicate buffer for A");
    assert_eq!(app.active, 0, "A focused");
    assert_eq!(
        String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
        "body a"
    );

    // Re-open B: focuses it again, edits intact.
    app.open_or_focus_buffer(selected_with("b.toml", Some("body b")));
    assert_eq!(app.buffers.len(), 2, "still two buffers");
    assert_eq!(app.active, 1);
    assert_eq!(
        String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
        "edited b",
        "B's in-memory edits survive the dedup-focus"
    );
}

/// Response/scroll/in-flight are now PER-ENDPOINT (see DECISIONS.md, PR-3a):
/// loading a different endpoint shows its OWN fresh `Idle` response, not the
/// previous endpoint's stale one. On master (global `response`), A's completed
/// response bled through under B until a send; this locks in the fix.
#[test]
fn loading_a_new_endpoint_shows_a_fresh_response_not_the_previous() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    // Endpoint A: drive it to a completed response with a non-zero scroll.
    app.open_or_focus_buffer(selected_with("a.toml", None));
    *app.response_mut() = ResponseState::Done {
        view: ResponseView::build(&response(), 1),
    };
    app.active_endpoint_buffer_mut().unwrap().geometry.scroll = 7;
    assert!(matches!(app.response(), ResponseState::Done { .. }));

    // Load a DIFFERENT endpoint B (no send): B gets its own fresh response.
    app.open_or_focus_buffer(selected_with("b.toml", None));
    let b = app.active_endpoint_buffer().unwrap();
    assert!(
        matches!(b.response, ResponseState::Idle),
        "B shows its own Idle response, not A's stale Done"
    );
    assert_eq!(
        b.geometry.scroll, 0,
        "B starts unscrolled, not at A's scroll"
    );
    assert!(b.in_flight.is_none(), "B has no in-flight from A");
}

/// `is_dirty` is derived per-`EndpointBuffer`: an in-memory body edit that
/// diverges from the buffer's own snapshot marks it dirty.
#[test]
fn is_dirty_is_per_endpoint_buffer() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
    assert!(!app.is_dirty());
    // Edit the body editor away from the snapshot → dirty.
    *app.test_editor() = EditorState::new(Lines::from("changed"));
    assert!(app.is_dirty(), "body edit diverging from snapshot is dirty");
    // Restore the exact snapshot text → clean again (derived, not a flag).
    *app.test_editor() = EditorState::new(Lines::from("orig"));
    assert!(!app.is_dirty(), "reverting the edit clears dirty");
}

/// Distinct buffers keep independent editor state: editing A, switching to B
/// and back leaves A's edit intact (each buffer owns its editor).
#[test]
fn distinct_buffers_keep_independent_editor_state() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("A")));
    app.open_or_focus_buffer(selected_with("b.toml", Some("B")));
    assert_eq!(app.buffers.len(), 2);
    app.active = 0;
    *app.test_editor() = EditorState::new(Lines::from("A edited"));
    app.active = 1;
    assert_eq!(
        String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
        "B"
    );
    app.active = 0;
    assert_eq!(
        String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
        "A edited",
        "A's edit survived the round-trip"
    );
}

/// Switching away from a buffer evicts its highlight cache, so total
/// cached-line memory is bounded by (active buffers × 64), not all buffers.
/// The buffer you land ON is untouched; a same-index focus never evicts.
#[test]
fn buffer_switch_evicts_inactive_highlight_cache() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", None)); // active = 0
    app.open_or_focus_buffer(selected_with("b.toml", None)); // active = 1

    // Seed a cache entry in BOTH buffers.
    for i in [0usize, 1] {
        let b = app.buffers[i].as_endpoint_mut().unwrap();
        b.highlight_cache.insert(i as u64, vec![Line::from("x")]);
        b.pending_highlight = Some(i as u64);
    }
    assert_eq!(app.active, 1);

    // Cycle to buffer 0: buffer 1 (the one we left) must be evicted; buffer 0
    // (the one we land on) keeps its cache.
    app.buffer_cycle(false); // 1 -> 0
    assert_eq!(app.active, 0);
    assert!(
        app.buffers[1]
            .as_endpoint()
            .unwrap()
            .highlight_cache
            .is_empty(),
        "left buffer's cache evicted"
    );
    assert!(
        app.buffers[1]
            .as_endpoint()
            .unwrap()
            .pending_highlight
            .is_none()
    );
    assert_eq!(
        app.buffers[0].as_endpoint().unwrap().highlight_cache.len(),
        1,
        "landed buffer keeps its cache"
    );

    // Re-focusing the SAME buffer is a no-op — its cache is not wiped.
    app.set_active_buffer(0);
    assert_eq!(
        app.buffers[0].as_endpoint().unwrap().highlight_cache.len(),
        1,
        "same-index focus does not evict"
    );

    // At most one buffer (the active one) ever holds a cache after switches,
    // so the total is bounded by active-buffers, not all-buffers.
    let with_cache = app
        .buffers
        .iter()
        .filter(|b| {
            b.as_endpoint()
                .is_some_and(|e| !e.highlight_cache.is_empty())
        })
        .count();
    assert_eq!(with_cache, 1, "only the active buffer retains a cache");
}

/// `buffer_cycle` wraps forward/backward and no-ops on an empty list.
#[test]
fn buffer_cycle_wraps_and_noops_on_empty() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.buffer_cycle(true); // no panic on empty
    assert_eq!(app.active, 0);

    for f in ["a.toml", "b.toml", "c.toml"] {
        app.open_or_focus_buffer(selected_with(f, None));
    }
    app.active = 0;
    app.buffer_cycle(true);
    assert_eq!(app.active, 1);
    app.buffer_cycle(true);
    app.buffer_cycle(true);
    assert_eq!(app.active, 0, "forward wraps 2 -> 0");
    app.buffer_cycle(false);
    assert_eq!(app.active, 2, "backward wraps 0 -> 2");
}

/// `new_active_after_close` neighbour rule across the closed-vs-active matrix.
#[test]
fn close_clean_new_active_matrix() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    let reopen = |app: &mut App| {
        app.buffers.clear();
        for f in ["a.toml", "b.toml", "c.toml"] {
            app.open_or_focus_buffer(selected_with(f, None));
        }
    };

    // closed < active: active shifts left.
    reopen(&mut app);
    app.active = 2;
    app.close_buffer(0);
    assert_eq!(app.buffers.len(), 2);
    assert_eq!(app.active, 1, "closed<active shifts active left");

    // closed == active (middle): right neighbour takes the slot.
    reopen(&mut app);
    app.active = 1;
    app.close_buffer(1);
    assert_eq!(app.active, 1, "closed==active picks the right neighbour");

    // closed > active: active unchanged.
    reopen(&mut app);
    app.active = 0;
    app.close_buffer(2);
    assert_eq!(app.active, 0, "closed>active leaves active");

    // close the tail while active is the tail: clamps to new last.
    reopen(&mut app);
    app.active = 2;
    app.close_buffer(2);
    assert_eq!(app.active, 1, "closing the active tail clamps to new last");

    // close the last remaining buffer -> empty, active 0.
    app.buffers.clear();
    app.open_or_focus_buffer(selected_with("only.toml", None));
    app.close_buffer(0);
    assert!(app.buffers.is_empty());
    assert_eq!(app.active, 0);
}

/// Closing a DIRTY buffer opens the discard confirm with `pending_close`
/// parked; `d` closes it, `Esc` keeps it.
#[test]
fn close_dirty_buffer_confirm_discard_and_cancel() {
    // d closes.
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
    *app.test_editor() = EditorState::new(Lines::from("edited"));
    assert!(app.is_dirty());
    app.close_buffer(0);
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges)
    ));
    assert!(matches!(app.pending_close, Some(PendingClose::One(_))));
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(app.buffers.is_empty(), "d discards + closes");
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.pending_close.is_none());

    // Esc keeps the buffer.
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
    *app.test_editor() = EditorState::new(Lines::from("edited"));
    app.close_buffer(0);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.buffers.len(), 1, "Esc keeps the dirty buffer");
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.pending_close.is_none());
}

/// Closing a CLEAN buffer skips the confirm entirely.
#[test]
fn close_clean_buffer_no_confirm() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", None));
    app.close_buffer(0);
    assert!(app.buffers.is_empty());
    assert!(
        matches!(app.mode, Mode::Normal),
        "clean close needs no confirm"
    );
}

/// close-all with two dirty buffers prompts one at a time; `d` on each drains
/// to empty. Clean buffers close immediately.
#[test]
fn close_all_two_dirty_sequential_prompts() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    // A dirty, B clean, C dirty.
    app.open_or_focus_buffer(selected_with("a.toml", Some("a")));
    app.active = 0;
    *app.test_editor() = EditorState::new(Lines::from("a edited"));
    app.open_or_focus_buffer(selected_with("b.toml", None));
    app.open_or_focus_buffer(selected_with("c.toml", Some("c")));
    app.active = 2;
    *app.test_editor() = EditorState::new(Lines::from("c edited"));

    app.close_all_buffers();
    // Clean B closed immediately; A + C queued behind the confirm.
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges)
    ));
    assert!(matches!(app.pending_close, Some(PendingClose::All(_))));
    assert_eq!(app.buffers.len(), 2, "B closed immediately");

    // Discard first dirty (A) -> prompt for C.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges)
    ));
    assert_eq!(app.buffers.len(), 1, "A closed, C remains");
    // Discard C -> empty.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(app.buffers.is_empty(), "queue drained");
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.pending_close.is_none());
}

/// Esc mid close-all queue aborts the rest: the already-closed clean/discarded
/// buffers stay closed, the still-dirty ones stay open.
#[test]
fn close_all_esc_mid_queue_aborts_remaining() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", Some("a")));
    app.active = 0;
    *app.test_editor() = EditorState::new(Lines::from("a edited"));
    app.open_or_focus_buffer(selected_with("c.toml", Some("c")));
    app.active = 1;
    *app.test_editor() = EditorState::new(Lines::from("c edited"));

    app.close_all_buffers();
    assert_eq!(app.buffers.len(), 2, "both dirty, both queued");
    // Discard A -> prompt for C.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.buffers.len(), 1, "A closed");
    // Esc aborts: C stays open.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.buffers.len(), 1, "C stays open after Esc");
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.pending_close.is_none());
}

/// `leader_sub_lookup(Tabs, …)` resolves the buffer actions.
#[test]
fn leader_tabs_submenu_binds() {
    let km = KeyMap::default();
    let lk = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    assert_eq!(
        km.leader_sub_lookup("tabs", lk('n')),
        Some(Action::BufferNext)
    );
    assert_eq!(
        km.leader_sub_lookup("tabs", lk('p')),
        Some(Action::BufferPrev)
    );
    assert_eq!(
        km.leader_sub_lookup("tabs", lk('x')),
        Some(Action::BufferClose)
    );
    assert_eq!(
        km.leader_sub_lookup(
            "tabs",
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)
        ),
        Some(Action::BufferCloseAll)
    );
    // Note #5: `<leader>t 1`..`9` resolve to FocusBufferIndex(n) — in the
    // tabs SUBMENU layer only (see `numbered_jump_does_not_shadow_request_...`).
    for n in 1..=9usize {
        let digit = char::from_digit(n as u32, 10).unwrap();
        assert_eq!(
            km.leader_sub_lookup("tabs", lk(digit)),
            Some(Action::FocusBufferIndex(n)),
            "<leader>t {n} → FocusBufferIndex({n})"
        );
    }
}

/// The tabs-submenu digit binds (`<leader>t <n>`) live ONLY in the `tabs`
/// submenu layer and must NOT shadow the Request-pane `1`..`4` tab-jump
/// overlay. The submenu resolves a digit to `FocusBufferIndex`, while
/// the Request `PaneCtx` overlay still resolves the same digit to its
/// `Tab1`..`Tab4` action — two independent keymap layers, no clash.
#[test]
fn numbered_jump_does_not_shadow_request_pane_digits() {
    let km = KeyMap::default();
    let lk = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    // Submenu layer: digit → FocusBufferIndex.
    assert_eq!(
        km.leader_sub_lookup("tabs", lk('3')),
        Some(Action::FocusBufferIndex(3))
    );
    // Request-pane overlay: the same digit still means Tab3 (Auth) there.
    assert_eq!(
        km.lookup_ctx(lk('3'), PaneCtx::Request),
        Some(Action::Tab3),
        "Request-pane digit overlay is untouched by the submenu binds"
    );
    // And `5`..`9` are NOT bound in the Request pane (only `1`..`4` are), so
    // the extra submenu digits can never leak a Request-pane action.
    assert_eq!(km.lookup_ctx(lk('5'), PaneCtx::Request), None);
}

/// `<leader>t 3` dispatches to `FocusBufferIndex(3)`, focusing the 3rd tab.
#[test]
fn focus_buffer_index_focuses_nth_tab() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.buffers = vec![
        Buffer::endpoint(selected_with("a.toml", None)),
        Buffer::endpoint(selected_with("b.toml", None)),
        Buffer::endpoint(selected_with("c.toml", None)),
    ];
    app.active = 0;
    // Dispatch the action the `<leader>t 3` bind resolves to.
    app.dispatch(Action::FocusBufferIndex(3), None).unwrap();
    assert_eq!(app.active, 2, "1-based jump: tab 3 → index 2");
    assert!(app.message.is_none(), "in-range jump sets no message");
}

/// Out-of-range `<leader>t <n>` (n > open count, and n == 0) is a graceful
/// no-op with a "no tab N" message — never a panic or a wrong-tab jump.
#[test]
fn focus_buffer_index_out_of_range_is_noop_with_message() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.buffers = vec![
        Buffer::endpoint(selected_with("a.toml", None)),
        Buffer::endpoint(selected_with("b.toml", None)),
    ];
    app.active = 1;
    // n = 5 > 2 open tabs → no-op + message, active unchanged.
    app.dispatch(Action::FocusBufferIndex(5), None).unwrap();
    assert_eq!(app.active, 1, "out-of-range never moves the active tab");
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("no tab 5")
    );
    // n = 0 is likewise a guarded no-op (defensive; not reachable via 1..9).
    app.message = None;
    app.dispatch(Action::FocusBufferIndex(0), None).unwrap();
    assert_eq!(app.active, 1);
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("no tab 0")
    );
}

/// `send_request`'s in-flight guard reads the ACTIVE buffer: with an
/// in-flight already on it, a second send reports "already in flight" and
/// does not overwrite the slot.
#[tokio::test]
async fn send_request_guards_on_active_buffer_in_flight() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    open_bare_endpoint(&mut app);
    set_active_in_flight(&mut app, 1);
    app.send_request();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("request already in flight — ctrl-c to cancel")
    );
    assert!(
        active_in_flight_is_some(&app),
        "existing in-flight untouched"
    );
}

/// `on_response` routes by `in_flight.generation` to the MATCHING buffer,
/// even when it is not the active one. Two buffers each carry an in-flight at
/// a distinct generation; the response lands on the right buffer only.
#[tokio::test]
async fn on_response_routes_by_generation_to_matching_buffer() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    // Two buffers pushed directly (Stage 2 shape; Stage 1 keeps 1, but the
    // routing scan must already be correct).
    app.buffers = vec![
        Buffer::endpoint(selected_with("a.toml", None)),
        Buffer::endpoint(selected_with("b.toml", None)),
    ];
    // Buffer 0 in-flight at gen 7; buffer 1 in-flight at gen 9.
    app.buffers[0].as_endpoint_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: tokio::spawn(async {}).abort_handle(),
        generation: 7,
        meta: meta(),
    });
    app.buffers[1].as_endpoint_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: tokio::spawn(async {}).abort_handle(),
        generation: 9,
        meta: meta(),
    });
    app.active = 0;

    // A response for gen 9 must land on buffer 1 (NOT the active buffer 0).
    app.on_response(9, Ok(response()), meta());
    assert!(
        matches!(
            app.buffers[1].as_endpoint().unwrap().response,
            ResponseState::Done { .. }
        ),
        "gen 9 lands on buffer 1"
    );
    assert!(app.buffers[1].as_endpoint().unwrap().in_flight.is_none());
    // Buffer 0 is untouched (still in-flight, still Idle).
    assert!(app.buffers[0].as_endpoint().unwrap().in_flight.is_some());
    assert!(matches!(
        app.buffers[0].as_endpoint().unwrap().response,
        ResponseState::Idle
    ));

    // An unknown generation is dropped without touching any buffer.
    app.on_response(42, Ok(response()), meta());
    assert!(app.buffers[0].as_endpoint().unwrap().in_flight.is_some());
}

/// Closing a clean buffer aborts its in-flight request before removal (no
/// history row written — silent, per the design).
#[tokio::test]
async fn close_buffer_aborts_its_in_flight() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.open_or_focus_buffer(selected_with("a.toml", None));
    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let aborted = handle.abort_handle();
    app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: aborted.clone(),
        generation: 1,
        meta: meta(),
    });
    app.close_buffer(0);
    assert!(app.buffers.is_empty(), "clean buffer closed");
    tokio::task::yield_now().await;
    assert!(
        aborted.is_finished(),
        "the in-flight task was aborted on close"
    );
}

/// `switch_workspace` drops every buffer (clearing all editor/response/dirty
/// state) and aborts any in-flight request.
#[tokio::test]
async fn switch_workspace_clears_buffers_and_aborts_in_flight() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir_a.path());
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    assert!(app.selected().is_some());
    // A never-completing in-flight task so "finished" unambiguously means
    // "aborted" (an empty task would finish on its own).
    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let aborted = handle.abort_handle();
    app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: aborted.clone(),
        generation: 1,
        meta: meta(),
    });

    let dir_b = tempfile::tempdir().unwrap();
    other_workspace_fixture(dir_b.path());
    app.switch_workspace(dir_b.path().to_path_buf()).unwrap();

    assert!(app.buffers.is_empty(), "all buffers dropped on switch");
    assert_eq!(app.active, 0);
    assert!(app.selected().is_none());
    // Let the abort propagate, then confirm the task is gone (not orphaned).
    tokio::task::yield_now().await;
    assert!(aborted.is_finished(), "the in-flight task was aborted");
}

/// `remap_buffers` removes a buffer whose file vanished (post-delete) and
/// clamps `active`.
#[test]
fn remap_buffers_removes_vanished_file_buffer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    let file = coll.join("ping.toml");
    std::fs::write(
            &file,
            "seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ping\"\n",
        )
        .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.guarded_load(PendingLoad::File(file.clone())).unwrap();
    assert_eq!(app.buffers.len(), 1);

    // The file disappears on disk, then a reload remaps.
    std::fs::remove_file(&file).unwrap();
    app.reload_explorer().unwrap();
    assert!(app.buffers.is_empty(), "vanished-file buffer is removed");
    assert_eq!(app.active, 0);
    assert!(app.selected().is_none());
}

/// Renaming the loaded endpoint repoints its buffer's file path + name +
/// snapshot name in place (path-based), preserving unsaved edits.
#[test]
fn rename_repoints_loaded_buffer_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    let file = coll.join("ping.toml");
    std::fs::write(
            &file,
            "seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ping\"\n",
        )
        .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    app.guarded_load(PendingLoad::File(file.clone())).unwrap();
    // Move the explorer cursor onto the endpoint row so rename targets it.
    app.explorer.select_file(&file).unwrap();
    // Make an unsaved edit that must survive the rename.
    *app.test_editor() = EditorState::new(Lines::from("draft body"));

    app.commit_rename("Pong".to_owned()).unwrap();

    let b = app
        .active_endpoint_buffer()
        .expect("buffer survives rename");
    assert_eq!(b.endpoint.endpoint.name, "Pong", "endpoint name repointed");
    assert_eq!(b.loaded_snapshot.name, "Pong", "snapshot name repointed");
    assert!(
        b.endpoint.file.ends_with("pong.toml"),
        "file path repointed (slugified), got {:?}",
        b.endpoint.file
    );
    assert_eq!(
        String::from(b.editor.lines.clone()),
        "draft body",
        "unsaved edit survives the in-place rename"
    );
}

// ---- B1: in-flight-on-quit history recording ----------------------------------

/// On quit, an in-flight request records an interrupted history row (no
/// status/duration), mirroring `cancel_request` — so a request the user quit
/// mid-flight isn't silently lost from history. Its task is aborted.
#[tokio::test]
async fn quit_records_in_flight_request_in_history() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.open_or_focus_buffer(selected_with("a.toml", None));
    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let aborted = handle.abort_handle();
    app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
        handle: aborted.clone(),
        generation: 1,
        meta: meta(),
    });

    app.record_inflight_on_quit();

    // An interrupted row (status None) was written for the request.
    let rows = app.history.as_ref().unwrap().recent(10).unwrap();
    assert_eq!(rows.len(), 1, "one interrupted history row recorded");
    assert_eq!(rows[0].url, "https://api.test/x");
    assert!(rows[0].status.is_none(), "interrupted → no status");
    // The in-flight slot was drained and its task aborted (runtime-safe on quit).
    assert!(!active_in_flight_is_some(&app));
    tokio::task::yield_now().await;
    assert!(
        aborted.is_finished(),
        "the in-flight task was aborted on quit"
    );
}

/// Every in-flight buffer is recorded on quit, not just the active one
/// (the response router is per-buffer, so quit must be too).
#[tokio::test]
async fn quit_records_all_in_flight_buffers() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.buffers = vec![
        Buffer::endpoint(selected_with("a.toml", None)),
        Buffer::endpoint(selected_with("b.toml", None)),
    ];
    for (i, generation) in [7u64, 9].into_iter().enumerate() {
        app.buffers[i].as_endpoint_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: tokio::spawn(async { std::future::pending::<()>().await }).abort_handle(),
            generation,
            meta: meta(),
        });
    }

    app.record_inflight_on_quit();

    let rows = app.history.as_ref().unwrap().recent(10).unwrap();
    assert_eq!(rows.len(), 2, "both in-flight buffers recorded");
    assert!(
        app.buffers
            .iter()
            .all(|b| b.as_endpoint().is_none_or(|e| e.in_flight.is_none()))
    );
}

/// Quit with nothing in flight writes no history row (no spurious markers).
#[tokio::test]
async fn quit_without_in_flight_writes_nothing() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.history = Some(HistoryStore::in_memory().unwrap());
    app.open_or_focus_buffer(selected_with("a.toml", None));
    app.record_inflight_on_quit();
    assert!(app.history.as_ref().unwrap().recent(10).unwrap().is_empty());
}

// ---- B3: sticky history-write-failure indicator -------------------------------

/// The sticky "history not recording" flag arms after N consecutive write
/// failures and clears on the next success. Driven through
/// `note_history_write`, which is exactly what `write_history` /
/// `write_load_summary` call on each SQLite insert outcome. (A real insert
/// failure can't be forced deterministically without exposing a test-only DB
/// seam on `HistoryStore`; the failure→sticky→clear contract lives entirely in
/// this counter, so it's tested directly here and rendered below.)
#[test]
fn history_failures_arm_sticky_flag_then_clear_on_success() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    assert!(!app.history_failing(), "clean session shows no flag");

    for _ in 0..App::HISTORY_FAIL_THRESHOLD {
        app.note_history_write(false);
    }
    assert!(app.history_failing(), "flag arms at the failure threshold");

    // Further failures keep it armed.
    app.note_history_write(false);
    assert!(app.history_failing());

    // A single success clears it.
    app.note_history_write(true);
    assert!(!app.history_failing(), "a successful write clears the flag");
}

/// A successful `write_history` (working store) never arms the flag — the
/// wiring feeds success into the counter (guards against a regression that
/// forgets to reset on the happy path).
#[test]
fn successful_write_history_leaves_flag_clear() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.history = Some(HistoryStore::in_memory().unwrap());
    // Pre-arm the counter, then a real successful insert must clear it.
    app.note_history_write(false);
    assert!(app.history_failing());
    app.write_history(&meta(), Some(200), None);
    assert!(
        !app.history_failing(),
        "a real successful insert resets the flag"
    );
    assert_eq!(app.history.as_ref().unwrap().recent(10).unwrap().len(), 1);
}

/// `<leader>r` reload re-reads `churl.toml` from disk: an out-of-band edit to
/// the manifest (another editor / a second churl instance) is picked up without
/// a restart. Rewrites the manifest name + workspace vars behind the app's back,
/// then asserts `reload_workspace` swaps `self.workspace` to the on-disk state.
#[test]
fn reload_rereads_manifest_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"before\"\n\n[vars]\nbase = \"old\"\n",
    )
    .unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("get.toml"),
        "seq = 0\nname = \"Get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://x/y\"\n",
    )
    .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "before");

    // Mutate the manifest out-of-band: rename the workspace and change a var.
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"after\"\n\n[vars]\nbase = \"new\"\n",
    )
    .unwrap();
    // Add a collection on disk too, so the explorer rebuild is exercised.
    churl_core::persistence::create_collection(dir.path(), "extra", dir.path()).unwrap();

    app.reload_workspace().unwrap();

    let manifest = app.workspace.as_ref().unwrap().manifest();
    assert_eq!(manifest.name, "after", "manifest name re-read from disk");
    assert_eq!(
        manifest.vars.get("base").map(String::as_str),
        Some("new"),
        "workspace vars re-read from disk"
    );
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("reloaded from disk")
    );
}

/// A reload requested while an editor is dirty DEFERS (refuses with a status
/// message) rather than discarding — the unsaved edit survives and the on-disk
/// change is NOT applied until the user saves. Mirrors the workspace-switch
/// dirty guard, but reload never destroys buffers, so it refuses in place.
#[test]
fn reload_while_dirty_defers_and_preserves_edit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    *app.test_editor() = EditorState::new(Lines::from("unsaved edit"));
    assert!(app.is_dirty());

    // Change the manifest on disk, then reload while dirty.
    std::fs::write(dir.path().join("churl.toml"), "name = \"changed\"\n").unwrap();
    app.reload_workspace().unwrap();

    // Refused: the dirty edit survives and the on-disk rename was NOT applied.
    assert!(
        app.is_dirty(),
        "the dirty edit must survive a refused reload"
    );
    assert_eq!(
        app.workspace.as_ref().unwrap().manifest().name,
        "demo",
        "the workspace was not swapped while dirty"
    );
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("unsaved changes — save before reloading from disk")
    );
}

#[test]
fn reload_with_corrupt_manifest_keeps_old_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"before\"\n\n[vars]\nbase = \"old\"\n",
    )
    .unwrap();
    let ws = open_workspace(dir.path()).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "before");

    // Corrupt the manifest on disk out-of-band (unterminated table header).
    std::fs::write(dir.path().join("churl.toml"), "name = \"before\"\n[vars\n").unwrap();

    // A failed re-open must not panic, must leave the old workspace intact, and
    // must surface the failure (fail loudly, never half-swap).
    app.reload_workspace().unwrap();
    assert_eq!(
        app.workspace.as_ref().unwrap().manifest().name,
        "before",
        "a corrupt reload keeps the old in-memory workspace"
    );
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.starts_with("failed to reload workspace")),
        "the reload failure is surfaced: {:?}",
        app.message.as_ref().map(|m| m.text.as_str())
    );
}

// ---- M7.9: variable inheritance (ancestor-chain resolver) ----

/// Loads the endpoint at `file` into the app (via the explorer) and returns it.
fn load_file(app: &mut App, file: &std::path::Path) -> SelectedEndpoint {
    let selected = app
        .explorer
        .select_file(file)
        .unwrap()
        .expect("endpoint loads");
    app.load_endpoint(selected.clone());
    selected
}

/// Var inheritance regression: a child collection's var OVERRIDES its parent's,
/// which OVERRIDES the root collection's — a single inherit-and-override chain
/// resolved by `build_resolver`. `who` is defined at all three levels (deepest
/// wins); `mid` ONLY on the parent and `from_root` ONLY on the root, and both must
/// still resolve at the leaf — so a broken leaf-only resolver (that skipped the
/// ancestor walk) would fail `mid`/`from_root`, not just `who`.
#[test]
fn m79_child_overrides_parent_overrides_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nwho = \"root\"\nfrom_root = \"R\"\n",
    )
    .unwrap();
    let parent = root.join("parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(
        parent.join("folder.toml"),
        "[vars]\nwho = \"parent\"\nmid = \"M\"\n",
    )
    .unwrap();
    let child = parent.join("child");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(child.join("folder.toml"), "[vars]\nwho = \"child\"\n").unwrap();
    let leaf = child.join("leaf.toml");
    std::fs::write(
        &leaf,
        "seq = 0\nname = \"Leaf\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{who}}/x\"\n",
    )
    .unwrap();

    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let selected = load_file(&mut app, &leaf);
    let resolver = app.build_resolver(&selected);
    assert_eq!(
        resolver.substitute("{{who}}"),
        "child",
        "the deepest collection var must win over parent and root"
    );
    assert_eq!(
        resolver.substitute("{{mid}}"),
        "M",
        "a parent-only var must resolve at the leaf (ancestor chain walked)"
    );
    assert_eq!(
        resolver.substitute("{{from_root}}"),
        "R",
        "a root-only var must resolve at the leaf (chain reaches the root)"
    );
}

/// Var inheritance regression: a var defined only at the root (or a parent)
/// is INHERITED by a deeper endpoint that does not redefine it.
#[test]
fn m79_deep_endpoint_inherits_root_and_parent_vars() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nfrom_root = \"R\"\n",
    )
    .unwrap();
    let parent = root.join("parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("folder.toml"), "[vars]\nfrom_parent = \"P\"\n").unwrap();
    let child = parent.join("child");
    std::fs::create_dir(&child).unwrap();
    let leaf = child.join("leaf.toml");
    std::fs::write(
        &leaf,
        "seq = 0\nname = \"Leaf\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{from_root}}/{{from_parent}}\"\n",
    )
    .unwrap();

    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let selected = load_file(&mut app, &leaf);
    let resolver = app.build_resolver(&selected);
    assert_eq!(
        resolver.substitute("{{from_root}}"),
        "R",
        "root var inherited"
    );
    assert_eq!(
        resolver.substitute("{{from_parent}}"),
        "P",
        "parent var inherited"
    );
}

/// Var inheritance regression: a ROOT endpoint sees ONLY the root collection's
/// vars — no sub-collection var leaks into it (there is no collection ancestor
/// besides the root).
#[test]
fn m79_root_endpoint_sees_only_root_vars() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nwho = \"root\"\n",
    )
    .unwrap();
    // A sub-collection that ALSO defines `who` — it must NOT reach the root endpoint.
    let sub = root.join("api");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("folder.toml"), "[vars]\nwho = \"api\"\n").unwrap();
    let ping = root.join("ping.toml");
    std::fs::write(
        &ping,
        "seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{who}}/ping\"\n",
    )
    .unwrap();

    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let selected = load_file(&mut app, &ping);
    // The root endpoint's owning collection is node 0 (the root).
    assert_eq!(selected.collection, 0);
    let resolver = app.build_resolver(&selected);
    assert_eq!(
        resolver.substitute("{{who}}"),
        "root",
        "a root endpoint resolves only the root collection's vars"
    );
}

// --- M7.12 creation gestures + curl auto-detect ---

/// Number of `*.toml` files directly under `dir` (endpoint/manifest files).
fn count_toml(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("toml"))
        .count()
}

/// `<leader>n` opens the destination picker (root first), not a bare name prompt.
#[test]
fn leader_new_endpoint_opens_destination_picker() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::NewEndpointPick, None).unwrap();
    assert!(matches!(app.mode, Mode::Palette));
    match &app.picker {
        Some(Picker::Destination { dirs, purpose, .. }) => {
            assert_eq!(*purpose, DestPurpose::CreateEndpoint);
            assert_eq!(
                dirs.first(),
                Some(&dir.path().to_path_buf()),
                "root is first"
            );
            assert!(
                dirs.iter().any(|d| d.ends_with("users")),
                "collections listed"
            );
        }
        other => panic!("expected a destination picker, got {other:?}"),
    }
}

/// `<leader>n` → pick root → name prompt → the endpoint lands at the chosen root.
/// The name prompt opens on the light single-line editor by default (same as
/// every other prompt purpose) — a plain non-curl paste stays there.
#[test]
fn destination_picker_creates_endpoint_at_chosen_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::NewEndpointPick, None).unwrap();
    // Accept the first destination (root).
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)));
    assert!(app.curl_prompt.is_none(), "opens single-line by default");
    assert!(
        app.pending_create_dir.is_some(),
        "picked destination carried"
    );
    app.handle_paste("health".to_string());
    assert!(app.curl_prompt.is_none(), "a plain paste stays single-line");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(
        dir.path().join("health.toml").exists(),
        "endpoint created at the picked root"
    );
    assert!(app.pending_create_dir.is_none(), "destination consumed");
}

/// `<leader>n` → single-line prompt → typing a name keystroke-by-keystroke →
/// Enter creates the endpoint by that name (no curl involved at all).
#[test]
fn leader_new_endpoint_single_line_typed_name_creates_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::NewEndpointPick, None).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap(); // accept root
    assert!(app.curl_prompt.is_none());
    for c in "status".chars() {
        press(&mut app, c);
    }
    enter(&mut app);
    assert!(
        dir.path().join("status.toml").exists(),
        "typed name created a plain endpoint"
    );
}

/// The single-line name prompt auto-detects a pasted curl: the paste expands it
/// into the multi-line editor (Normal mode, seeded with the paste), and
/// submitting from there imports + auto-names the endpoint instead of creating
/// a blank one.
#[test]
fn name_prompt_auto_imports_pasted_curl() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)));
    assert!(app.curl_prompt.is_none(), "opens single-line by default");
    app.handle_paste("curl https://api.test/health".to_string());
    assert!(
        app.curl_prompt.is_some(),
        "curl paste expands to multi-line"
    );
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "paste-switch lands in Normal mode, ready to review/submit"
    );
    assert!(app.prompt_buffer_is_curl(), "buffer reads as curl");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    // Lands in the cursor's collection (the `users` row), auto-named `/health`.
    let file = dir.path().join("users").join("health.toml");
    assert!(
        file.exists(),
        "curl imported + auto-named from the URL path"
    );
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(ep.request.url, "https://api.test/health");
}

/// Importing a curl with a real Bearer token captures it into a RAM-only Session
/// var so `{{token}}` resolves and the endpoint is sendable — while the endpoint
/// file itself keeps only the placeholder (no secret on disk).
#[test]
fn importing_curl_captures_bearer_token_into_session_var() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    assert!(app.session_vars().is_empty(), "no session vars yet");
    app.begin_new_endpoint();
    app.handle_paste(
        "curl https://api.test/me -H 'Authorization: Bearer v4.public.REALTOKEN'".to_string(),
    );
    // The curl paste already switched to the multi-line editor in Normal mode.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    // Session var `token` holds the REAL value…
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("v4.public.REALTOKEN"),
        "real token captured into the session store"
    );
    // …but the persisted endpoint keeps only the placeholder.
    let file = dir.path().join("users").join("me.toml");
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(
        ep.request.auth,
        Some(churl_core::model::Auth::Bearer {
            token: "{{token}}".to_owned()
        }),
        "endpoint file holds the placeholder, not the secret"
    );
}

/// A bracketed paste routes the whole multi-line curl into the multi-line
/// new-endpoint editor (newlines normalised from the terminal's bare CR to LF)
/// so the existing submit → `import_curl` path parses it — the real fix for
/// the "unbalanced quotes / multiple URLs" bug (a per-key stream would have
/// submitted early on the first embedded newline).
#[test]
fn handle_paste_fills_prompt_and_imports_multiline_curl() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)));
    // As a real terminal / tmux delivers a bracketed paste: bare CR line breaks
    // and curl's `\[\]` glob-escaping of an array param.
    let pasted = "curl 'https://api.test/orders?fields\\[\\]=a&fields\\[\\]=b' \\\r  -H 'accept: application/json'";
    app.handle_paste(pasted.to_string());
    assert!(
        app.curl_prompt.is_some(),
        "curl paste expands to multi-line"
    );
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "paste-switch lands in Normal mode"
    );
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(
        buf.contains("fields\\[\\]=a"),
        "raw buffer keeps curl's escaping until import"
    );
    assert!(
        buf.contains('\n') && !buf.contains('\r'),
        "CR line breaks normalised to LF in the buffer"
    );
    assert!(app.prompt_buffer_is_curl(), "buffer reads as curl");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    let file = dir.path().join("users").join("orders.toml");
    assert!(file.exists(), "multi-line curl imported to an endpoint");
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(
        ep.request.url, "https://api.test/orders?fields[]=a&fields[]=b",
        "continuations collapsed and brackets unescaped"
    );
}

/// The whole point of upgrading the new-endpoint prompt to the multi-line vim
/// editor: after a paste, the user can navigate *across lines* (`k`/`j`) and
/// edit a value on a different line than where the paste left the cursor,
/// then submit — the same curl-detect → import path picks up the edit.
#[test]
fn new_endpoint_multiline_editor_edits_across_lines_before_submit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    let pasted = "curl 'https://api.test/orders' \\\r  -H 'X-Custom: old'";
    app.handle_paste(pasted.to_string());
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "curl paste-switch lands in Normal mode, ready to navigate"
    );
    // Walk up to the first line and back down to the header line —
    // exercising real cross-line motion the single-line editor could never do.
    press(&mut app, 'k'); // up to (or already at) the `curl …` line
    press(&mut app, 'j'); // back down to the `-H 'X-Custom: old'` line
    press(&mut app, '$'); // end of line: the closing quote
    press(&mut app, 'h'); // 'd'
    press(&mut app, 'h'); // 'l'
    press(&mut app, 'h'); // 'o' — start of "old"
    press(&mut app, 'x'); // delete o/l/d one at a time
    press(&mut app, 'x');
    press(&mut app, 'x');
    press(&mut app, 'i'); // insert before the (now-adjacent) closing quote
    for c in "new".chars() {
        press(&mut app, c);
    }
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(
        buf.contains("X-Custom: new"),
        "edit landed on the header line, not the URL line: {buf:?}"
    );
    esc(&mut app); // Insert -> Normal: only Normal-mode Enter submits
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    let file = dir.path().join("users").join("orders.toml");
    assert!(file.exists(), "edited multi-line curl still imported");
    let ep = persistence::load_endpoint(&file).unwrap();
    let header = ep
        .request
        .headers
        .iter()
        .find(|h| h.name == "X-Custom")
        .expect("custom header present");
    assert_eq!(
        header.value, "new",
        "in-editor edit made it into the import"
    );
}

/// A plain (non-curl) name typed directly into the multi-line editor —
/// keystroke by keystroke into the light single-line editor — still falls
/// through to a plain endpoint (the everyday `<leader>n`/`n` fast path).
#[test]
fn new_endpoint_single_line_plain_name_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    assert!(app.curl_prompt.is_none(), "opens single-line by default");
    for c in "ping".chars() {
        press(&mut app, c);
    }
    assert!(!app.prompt_buffer_is_curl(), "a plain name is not curl");
    enter(&mut app); // single-line prompts: Enter always commits
    let file = dir.path().join("users").join("ping.toml");
    assert!(file.exists(), "plain name created a plain endpoint");
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(ep.request.url, "", "plain endpoint has no URL to import");
}

/// A plain (non-curl) name typed into the expanded multi-line editor (reached
/// via `Ctrl-e`) — keystroke by keystroke, not pasted — still falls through to
/// a plain endpoint: the multi-line editor's own plain-name fallback is
/// unchanged by the two-stage prompt.
#[test]
fn new_endpoint_multiline_editor_plain_name_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(app.curl_prompt.is_some(), "ctrl-e expands to multi-line");
    for c in "ping".chars() {
        press(&mut app, c);
    }
    assert!(!app.prompt_buffer_is_curl(), "a plain name is not curl");
    esc(&mut app); // Insert -> Normal: only Normal-mode Enter submits
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    let file = dir.path().join("users").join("ping.toml");
    assert!(file.exists(), "plain name created a plain endpoint");
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(ep.request.url, "", "plain endpoint has no URL to import");
}

/// Vim-faithful: Insert-mode Enter in the multi-line curl prompt is a plain
/// newline (edtui's own Insert-mode binding), never a submit — only
/// Normal-mode Enter commits. Distinct from the single-line prompts, where
/// Enter always commits. Reached via `Ctrl-e`, which expands in Insert mode.
#[test]
fn new_endpoint_editor_insert_enter_adds_line_without_submitting() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let before = count_toml(dir.path());
    app.begin_new_endpoint();
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Insert,
        "ctrl-e expands into insert mode"
    );
    for c in "ping".chars() {
        press(&mut app, c);
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)),
        "Insert-mode Enter must not submit the prompt"
    );
    assert_eq!(count_toml(dir.path()), before, "no endpoint created");
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(
        buf.contains("ping") && buf.contains('\n'),
        "Insert-mode Enter inserted a newline into the buffer: {buf:?}"
    );
}

/// `o` in Normal mode opens a line below the cursor and switches to Insert —
/// edtui's own default binding, exercised here through the new-endpoint
/// prompt's key routing (nothing in `handle_curl_prompt_key` or `vim_ext`
/// intercepts `o`, so it falls through to edtui unmodified).
#[test]
fn new_endpoint_editor_normal_o_opens_line_below() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("curl https://api.test/health".to_string());
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "curl paste-switch lands in Normal mode"
    );
    press(&mut app, 'o');
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Insert,
        "`o` opens a line and drops into insert mode"
    );
    for c in "-H 'X-New: 1'".chars() {
        press(&mut app, c);
    }
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(
        buf.contains("curl https://api.test/health"),
        "original line kept: {buf:?}"
    );
    assert!(
        buf.contains("-H 'X-New: 1'"),
        "text typed after `o` landed on the opened line: {buf:?}"
    );
    assert!(
        buf.contains('\n'),
        "`o` opened a genuinely new line: {buf:?}"
    );
}

/// FIX (P2-1) regression: every edtui `EditorState` churl constructs (curl
/// prompt, URL popup, Body editor, render-only placeholder editor) must be
/// wired to edtui's in-memory clipboard via the `new_editor_state` helper
/// (`app/mod.rs`), not left on edtui's arboard/OS-clipboard default — that
/// default is a single shared OS handle, and concurrent access to it from
/// more than one live editor (e.g. this very test suite's parallel threads)
/// has been observed to segfault.
///
/// A live yank/paste round-trip can't discriminate reverted-vs-fixed code
/// here: `edtui::clipboard`'s arboard-backed `Default` impl silently falls
/// back to an internal, non-shared clipboard whenever no display is
/// available, and CI runs headless — so with or without the fix, headless
/// tests observe the same paste behaviour. `EditorState`'s clipboard field is
/// also `pub(crate)` inside edtui, so there is no supported way to ask a
/// constructed instance which backend it holds. Absent any environment- or
/// introspection-based way to observe the difference at runtime, this
/// asserts the source directly: the helper wires `InternalClipboard`, and
/// every real construction site routes through it rather than calling
/// `EditorState::new` on its own.
#[test]
fn every_editor_construction_uses_internal_clipboard_helper() {
    let mod_rs = include_str!("mod.rs");
    let state_rs = include_str!("state.rs");
    let editing_rs = include_str!("handlers/editing.rs");
    let render_rs = include_str!("render.rs");

    let helper_start = mod_rs
        .find("fn new_editor_state(text: &str) -> EditorState {")
        .expect("new_editor_state helper must exist in app/mod.rs");
    let helper_body = &mod_rs[helper_start..];
    let helper_end = helper_body
        .find("\n}\n")
        .expect("new_editor_state helper body must be closed");
    let helper_body = &helper_body[..helper_end];
    assert!(
        helper_body.contains("set_clipboard") && helper_body.contains("InternalClipboard"),
        "new_editor_state must wire edtui's in-memory clipboard: {helper_body:?}"
    );

    // The helper's own construction is the ONLY direct `EditorState::new(` in
    // mod.rs — every other site (there or in sibling files) must go through it.
    assert_eq!(
        mod_rs.matches("EditorState::new(").count(),
        1,
        "only new_editor_state may construct EditorState directly in app/mod.rs"
    );
    assert!(
        !state_rs.contains("EditorState::new("),
        "EndpointBuffer::new (Body editor) must build its editor via new_editor_state"
    );
    assert!(
        !editing_rs.contains("EditorState::new("),
        "begin_url_popup (URL popup editor) must build its editor via new_editor_state"
    );
    assert!(
        !render_rs.contains("EditorState::new(") && !render_rs.contains("EditorState::default("),
        "the render-only placeholder editor must build via new_editor_state, not \
         EditorState::new/default (both open a real arboard OS-clipboard handle)"
    );
    assert!(
        state_rs.contains("new_editor_state(body)"),
        "EndpointBuffer::new must call new_editor_state"
    );
    assert!(
        editing_rs.contains("new_editor_state(url.as_str())"),
        "begin_url_popup must call new_editor_state"
    );
    assert!(
        render_rs.contains(r#"new_editor_state("")"#),
        "the render-only placeholder editor must call new_editor_state"
    );
}

/// P1 regression: enabling bracketed paste globally must NOT silently drop a
/// paste into a request-row field. Pasting a token into a Headers VALUE field
/// (a core action) still inserts — `handle_paste` mirrors `handle_normal_key`'s
/// field-edit routing.
#[test]
fn paste_into_request_row_value_field_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.guarded_load(PendingLoad::Row(1)).unwrap();
    app.set_focus(Pane::Request);
    // Open a value-field edit seeded with a prefix, as `a`/edit would.
    if let Some(b) = app.active_endpoint_buffer_mut() {
        b.tabs.active = RequestTab::Headers;
        b.tabs.editing = Some(FieldEdit {
            row: 0,
            field: EditField::Value,
            editor: LineEditor::new("Bearer "),
        });
    }
    assert!(app.tabs_editing_active(), "a field edit is active");
    app.handle_paste("token-123".to_string());
    let text = app
        .active_endpoint_buffer()
        .unwrap()
        .tabs
        .editing
        .as_ref()
        .unwrap()
        .editor
        .text();
    assert_eq!(
        text, "Bearer token-123",
        "paste inserted at the cursor of the value field"
    );
}

/// P1 regression: a paste into an open fuzzy picker's filter appends to the
/// query (and refilters), rather than vanishing.
#[test]
fn paste_into_picker_filter_appends_query() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    // `<leader><leader>` opens the endpoint/request search picker.
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(app.mode, Mode::Search), "search picker open");
    app.handle_paste("user".to_string());
    assert_eq!(
        app.picker_state().unwrap().query,
        "user",
        "pasted term appended to the picker filter"
    );
}

/// A curl that fails to parse is fail-loud: nothing is created and the prompt
/// re-opens in the multi-line editor (not the single-line default) with the
/// buffer intact, since there's curl content to fix.
#[test]
fn name_prompt_curl_parse_failure_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let before = count_toml(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("curl".to_string()); // no URL → parse error, but still expands (starts_with "curl")
    assert!(
        app.curl_prompt.is_some(),
        "curl paste expands to multi-line"
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)),
        "prompt stays open on a curl parse failure"
    );
    assert!(
        app.curl_prompt.is_some(),
        "re-opens in the multi-line editor, not single-line — there's curl content to fix"
    );
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "re-opens in Normal mode"
    );
    assert!(
        app.curl_prompt.as_ref().unwrap().text().contains("curl"),
        "buffer intact after the failed parse"
    );
    assert_eq!(count_toml(dir.path()), before, "no endpoint created");
}

/// The retired `PasteCurl` action is now an alias that opens the unified
/// new-endpoint prompt (which auto-detects a subsequently pasted curl) — on the
/// same light single-line default as `NewEndpoint` itself.
#[test]
fn paste_curl_action_opens_unified_new_endpoint_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.dispatch(Action::PasteCurl, None).unwrap();
    assert!(
        matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)),
        "PasteCurl aliases the unified new-endpoint prompt"
    );
    assert!(
        app.curl_prompt.is_none(),
        "opens single-line by default, same as NewEndpoint"
    );
}

/// A multi-line paste that does NOT start with `curl` still expands to the
/// multi-line editor — the OR half of the trigger rule (curl-prefixed OR
/// contains a newline), not just the curl-prefix half.
#[test]
fn paste_with_newline_but_no_curl_prefix_still_expands_to_multiline() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    assert!(app.curl_prompt.is_none());
    app.handle_paste("first line\nsecond line".to_string());
    assert!(
        app.curl_prompt.is_some(),
        "a multi-line paste expands even without a `curl` prefix"
    );
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal
    );
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(buf.contains("first line") && buf.contains("second line"));
}

/// A plain single-line non-curl paste stays on the light single-line editor —
/// it lands in the name as if typed, no expansion.
#[test]
fn paste_plain_word_stays_single_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("foo".to_string());
    assert!(
        app.curl_prompt.is_none(),
        "a short non-curl single-line paste never expands"
    );
    assert_eq!(
        app.prompt_editor.text(),
        "foo",
        "pasted word lands in the name, same as typing it"
    );
}

/// A curl-lookalike word (no word boundary after `curl`) stays on the
/// single-line editor, same as any other plain word — the expand trigger now
/// shares `looks_like_curl` with the submit-time import check instead of a
/// bare `starts_with("curl")`, so `curly`/`curl-metrics` no longer wrongly
/// expand into the multi-line editor.
#[test]
fn paste_curl_lookalike_word_stays_single_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("curly".to_string());
    assert!(
        app.curl_prompt.is_none(),
        "a `curl`-prefixed but non-curl word never expands"
    );
    assert_eq!(
        app.prompt_editor.text(),
        "curly",
        "pasted word lands in the name, same as typing it"
    );
}

/// When the pasted text itself is a curl, the seed is the curl ALONE — any
/// already-typed prefix is discarded, not prepended. Prepending it would
/// produce `<prefix>curl …`, which fails `looks_like_curl` on submit and
/// silently degrades the import into a plain endpoint (see
/// `typed_prefix_then_curl_paste_still_imports_on_submit` for the submit-time
/// proof).
#[test]
fn paste_switch_discards_prior_typed_prefix_when_pasted_curl() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    for c in "myapi-".chars() {
        press(&mut app, c);
    }
    app.handle_paste("curl https://api.test/health".to_string());
    assert!(app.curl_prompt.is_some());
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert_eq!(
        buf, "curl https://api.test/health",
        "the typed prefix is discarded, not merged into the seed: {buf:?}"
    );
}

/// The typed-prefix-then-curl-paste sequence, taken all the way through
/// submit: the endpoint must IMPORT (method/URL from the curl), not fall
/// through to a plain endpoint named after the mangled `<prefix>curl …`
/// string — the actual P2-3 regression (`paste_switch_carries_prior_typed_
/// text_into_seed` above only ever built the buffer, it never submitted).
#[test]
fn typed_prefix_then_curl_paste_still_imports_on_submit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    for c in "myapi-".chars() {
        press(&mut app, c);
    }
    app.handle_paste("curl https://api.test/health".to_string());
    assert!(
        app.prompt_buffer_is_curl(),
        "buffer reads as curl despite the earlier typed prefix"
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    // Auto-named from the URL path, same as a prefix-free curl paste — proof
    // the typed prefix never reached the import (a mangled seed would have
    // failed `looks_like_curl` and created a plain `myapi-curl…` endpoint
    // instead).
    let file = dir.path().join("users").join("health.toml");
    assert!(
        file.exists(),
        "typed prefix + curl paste still imports, not a plain endpoint"
    );
    let ep = persistence::load_endpoint(&file).unwrap();
    assert_eq!(ep.request.url, "https://api.test/health");
}

/// The OTHER branch — a multi-line paste that is NOT a curl — still prefixes
/// prior typed text (it's name/notes content, not an import): the two-branch
/// split only changes seed-construction for the curl case.
#[test]
fn paste_switch_prepends_prior_typed_text_for_plain_multiline() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    for c in "notes-".chars() {
        press(&mut app, c);
    }
    app.handle_paste("first line\nsecond line".to_string());
    assert!(app.curl_prompt.is_some());
    let buf = app.curl_prompt.as_ref().unwrap().text();
    assert!(
        buf.starts_with("notes-first line"),
        "prior typed text still prefixes a non-curl multi-line paste: {buf:?}"
    );
}

/// A second bracketed paste that arrives while the multi-line curl editor
/// still sits in Normal mode (right after the paste-switch) must not land as
/// a vim Normal-mode paste-after one char off — gated exactly like the URL
/// popup, so it's dropped in Normal mode and lands correctly once the editor
/// is insert-capable.
#[test]
fn curl_prompt_second_paste_gated_to_insert_capable_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("curl https://api.test/orders".to_string());
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Normal,
        "curl paste-switch lands in Normal mode"
    );
    let before = app.curl_prompt.as_ref().unwrap().text();
    app.handle_paste("EXTRA".to_string());
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().text(),
        before,
        "a paste while still in Normal mode is dropped, not misplaced"
    );
    press(&mut app, 'i'); // Normal -> Insert
    app.handle_paste("EXTRA".to_string());
    assert!(
        app.curl_prompt.as_ref().unwrap().text().contains("EXTRA"),
        "once insert-capable, the paste lands at the cursor"
    );
}

/// `Ctrl-e` in the single-line `NewEndpoint` prompt expands to the multi-line
/// editor in Insert mode, seeded with whatever was already typed — covers the
/// non-empty prior-text case (the empty case is covered by
/// `new_endpoint_editor_insert_enter_adds_line_without_submitting` and
/// `new_endpoint_multiline_editor_plain_name_falls_through`).
#[test]
fn ctrl_e_expands_with_prior_typed_text_as_seed() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    for c in "orders".chars() {
        press(&mut app, c);
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(app.curl_prompt.is_some(), "ctrl-e expands to multi-line");
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().editor.mode,
        EditorMode::Insert,
        "ctrl-e expands into insert mode, ready to compose a curl by hand"
    );
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().text(),
        "orders",
        "whatever was already typed seeds the multi-line buffer"
    );
}

/// `Ctrl-e` is a no-op once the editor is already expanded — nothing to expand
/// to, and it must not do anything surprising (e.g. it is not bound to
/// anything in `handle_curl_prompt_key`/`vim_ext`, so it simply falls through
/// unhandled).
#[test]
fn ctrl_e_is_a_noop_once_already_multiline() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.begin_new_endpoint();
    app.handle_paste("curl https://api.test/health".to_string());
    assert!(app.curl_prompt.is_some());
    let before = app.curl_prompt.as_ref().unwrap().text();
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(app.curl_prompt.is_some(), "still expanded");
    assert_eq!(
        app.curl_prompt.as_ref().unwrap().text(),
        before,
        "ctrl-e does not mutate the multi-line buffer"
    );
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::NewEndpoint)));
}

/// `Ctrl-e`'s interception is scoped ONLY to the `NewEndpoint` single-line
/// sub-state — every other prompt purpose keeps `LineEditor`'s own `Ctrl-e` =
/// end-of-line binding (readline-style, paired with `Ctrl-a`).
#[test]
fn ctrl_e_in_other_prompt_purposes_still_moves_cursor_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.open_prompt(PromptPurpose::Rename, "hello");
    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.prompt_editor.cursor(), 0, "cursor moved off the end");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        app.prompt_editor.cursor(),
        "hello".chars().count(),
        "ctrl-e still moves the cursor to end-of-line outside NewEndpoint"
    );
    assert!(
        app.curl_prompt.is_none(),
        "no expansion outside the NewEndpoint purpose"
    );
    assert!(matches!(app.mode, Mode::Prompt(PromptPurpose::Rename)));
}

// --- M7.12 tree CRUD wiring ---

/// Reorder down on a selected endpoint swaps its `seq` with the next sibling.
#[test]
fn move_down_reorders_selected_endpoint_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let users = dir.path().join("users");
    persistence::create_endpoint(&users, "beta").unwrap(); // seq 1, after get (seq 0)
    app.reload_explorer().unwrap();
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1; // the first endpoint under `users` (Get user)
    assert_eq!(app.explorer.selected_kind(), Some(RowKind::Endpoint));
    app.dispatch(Action::MoveDown, None).unwrap();
    let names: Vec<String> = persistence::Collection {
        name: "users".into(),
        path: users.clone(),
    }
    .endpoints()
    .unwrap()
    .into_iter()
    .map(|(_, e)| e.name)
    .collect();
    assert_eq!(
        names,
        vec!["beta", "Get user"],
        "endpoint order swapped on disk"
    );
}

/// Reorder up at the top of a group reports the edge instead of a silent no-op.
#[test]
fn reorder_up_at_top_reports_already_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    persistence::create_endpoint(&dir.path().join("users"), "beta").unwrap();
    app.reload_explorer().unwrap();
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1; // the first endpoint
    app.dispatch(Action::MoveUp, None).unwrap();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("already first")
    );
}

/// Move-to an endpoint (app handler) relocates the file and rewrites the
/// sequence steps that referenced it.
#[test]
fn relocate_endpoint_move_rewrites_sequence_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let root = dir.path();
    let archive = persistence::create_collection(root, "archive", root).unwrap();
    std::fs::create_dir_all(root.join("sequences")).unwrap();
    std::fs::write(
        root.join("sequences/flow.toml"),
        "name = \"flow\"\n\n[[step]]\nseq = 0\nendpoint = \"users/get.toml\"\n",
    )
    .unwrap();
    let src = root.join("users").join("get.toml");
    app.relocate_endpoint(src.clone(), archive.clone(), true)
        .unwrap();
    assert!(!src.exists(), "source endpoint moved away");
    assert!(archive.join("get.toml").exists(), "endpoint at destination");
    let seq = persistence::load_sequence(&root.join("sequences/flow.toml")).unwrap();
    assert_eq!(
        seq.steps[0].endpoint, "archive/get.toml",
        "referencing step repointed by the move"
    );
}

/// Copy-to leaves the original in place and never rewrites references.
#[test]
fn relocate_endpoint_copy_keeps_original() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let root = dir.path();
    let archive = persistence::create_collection(root, "archive", root).unwrap();
    let src = root.join("users").join("get.toml");
    app.relocate_endpoint(src.clone(), archive.clone(), false)
        .unwrap();
    assert!(src.exists(), "copy leaves the original endpoint");
    assert!(
        archive.join("get.toml").exists(),
        "copy present at destination"
    );
}

/// Render-order fix: collections render before a node's own endpoints at the
/// root, matching the nested order.
#[test]
fn root_renders_collections_before_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    std::fs::create_dir(root.join("zed")).unwrap();
    std::fs::write(
        root.join("zed/x.toml"),
        "name = \"X\"\n\n[request]\nmethod = \"GET\"\nurl = \"\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("aaa.toml"),
        "name = \"Aaa\"\n\n[request]\nmethod = \"GET\"\nurl = \"\"\n",
    )
    .unwrap();
    let ws = open_workspace(root).unwrap();
    let app = App::new(ws, KeyMap::default()).unwrap();
    let rows = app.explorer.rows();
    let coll_idx = rows.iter().position(|r| r.kind == RowKind::Collection);
    let ep_idx = rows.iter().position(|r| r.kind == RowKind::Endpoint);
    assert_eq!(rows[0].name, "zed", "the collection sorts first");
    assert!(
        coll_idx < ep_idx,
        "collections render before root-level endpoints"
    );
}

/// Duplicate on a selected endpoint creates a suffixed sibling in the same
/// collection.
#[test]
fn duplicate_selected_endpoint_creates_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1; // the endpoint under `users`
    assert_eq!(app.explorer.selected_kind(), Some(RowKind::Endpoint));
    app.dispatch(Action::Duplicate, None).unwrap();
    let n = count_toml(&dir.path().join("users"));
    assert_eq!(n, 2, "duplicate created a second endpoint file");
}

// --- M7.12 interactive set-session-var (env editor) ---

/// Drives the env editor to the Session group and sets a var `name=value`
/// through the real key seam (`G` to the Session scope, `a`, name, Enter, value,
/// Enter).
fn set_session_var(app: &mut App, name: &str, value: &str) {
    let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.handle_key(k('G')).unwrap(); // jump to the last scope (Session)
    app.handle_key(k('a')).unwrap(); // open the name prompt
    for c in name.chars() {
        app.handle_key(k(c)).unwrap();
    }
    app.handle_key(enter).unwrap(); // commit name → value phase
    for c in value.chars() {
        app.handle_key(k(c)).unwrap();
    }
    app.handle_key(enter).unwrap(); // commit value → SetSessionVar
}

#[test]
fn set_session_var_resolves_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.open_env_editor();
    assert!(matches!(app.mode, Mode::EnvEditor(_)));
    set_session_var(&mut app, "token", "abc123");
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("abc123")
    );
    // A standalone `{{token}}` resolves from the in-memory session store.
    assert_eq!(app.build_env_resolver().substitute("{{token}}"), "abc123");
}

#[test]
fn set_session_var_overwrite_is_last_write_wins() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.open_env_editor();
    set_session_var(&mut app, "token", "first");
    set_session_var(&mut app, "token", "second");
    assert_eq!(
        app.session_vars().get("token").map(String::as_str),
        Some("second"),
        "setting an existing name replaces it"
    );
}

#[test]
fn set_session_var_empty_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.open_env_editor();
    let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    app.handle_key(k('G')).unwrap();
    app.handle_key(k('a')).unwrap();
    // Enter with an empty name is fail-loud: nothing written, prompt stays open.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(
        app.session_vars().is_empty(),
        "no var created on empty name"
    );
    match &app.mode {
        Mode::EnvEditor(ed) => assert!(
            ed.session_input.is_some(),
            "the name prompt stays open after an empty-name reject"
        ),
        _ => panic!("still in the env editor"),
    }
}

#[test]
fn delete_session_var_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    app.open_env_editor();
    set_session_var(&mut app, "token", "abc");
    assert!(!app.session_vars().is_empty());
    // Back on the Session scope, `d` deletes the selected (only) session var.
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        app.session_vars().is_empty(),
        "delete removed the session var"
    );
}

/// Every regular file under `dir`, recursively.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn session_var_never_touches_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let root = dir.path();
    app.open_env_editor();
    let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    // Edit a REAL workspace var so the forced save below actually writes churl.toml
    // (a no-change save would write nothing and prove nothing).
    app.handle_key(k('l')).unwrap(); // ScopeList → VarRows on the Workspace scope
    app.handle_key(k('a')).unwrap(); // add row → name edit
    for c in "wsvar".chars() {
        app.handle_key(k(c)).unwrap();
    }
    app.handle_key(enter).unwrap(); // commit name → value edit
    for c in "wsval".chars() {
        app.handle_key(k(c)).unwrap();
    }
    app.handle_key(enter).unwrap(); // commit value
    app.handle_key(k('h')).unwrap(); // back to ScopeList
    // Stash a session secret, then force a full save (`w`).
    set_session_var(&mut app, "sekret_token", "s3cr3tV4LUE");
    app.handle_key(k('w')).unwrap();
    // (a) the workspace var was saved; the session name/value never reach churl.toml.
    let manifest = std::fs::read_to_string(root.join("churl.toml")).unwrap();
    assert!(manifest.contains("wsvar"), "the workspace var was saved");
    assert!(
        !manifest.contains("sekret_token") && !manifest.contains("s3cr3tV4LUE"),
        "no session name/value in churl.toml: {manifest}"
    );
    // (b) the session name AND value appear in NO file anywhere in the workspace.
    for path in walk_files(root) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            assert!(
                !text.contains("sekret_token"),
                "session var NAME leaked into {path:?}"
            );
            assert!(
                !text.contains("s3cr3tV4LUE"),
                "session var VALUE leaked into {path:?}"
            );
        }
    }
    assert_eq!(
        app.session_vars().get("sekret_token").map(String::as_str),
        Some("s3cr3tV4LUE"),
        "the value is held only in the in-memory store"
    );
}

/// A rename whose sequence-step rewrite fails surfaces the error (fail-loud) —
/// not a false "renamed" success with a stranded sequence.
#[cfg(unix)]
#[test]
fn rename_surfaces_sequence_rewrite_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let root = dir.path();
    std::fs::create_dir_all(root.join("sequences")).unwrap();
    std::fs::write(
        root.join("sequences/flow.toml"),
        "name = \"flow\"\n\n[[step]]\nseq = 0\nendpoint = \"users/get.toml\"\n",
    )
    .unwrap();
    // sequences/ read-only: the rename lands, but the step rewrite's save fails.
    std::fs::set_permissions(
        root.join("sequences"),
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    assert_eq!(app.explorer.selected_kind(), Some(RowKind::Endpoint));
    app.commit_rename("fetched".to_owned()).unwrap();
    std::fs::set_permissions(
        root.join("sequences"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let msg = app
        .message
        .as_ref()
        .map(|m| m.text.as_str())
        .unwrap_or_default();
    assert!(
        msg.starts_with("error:"),
        "the rename surfaces the rewrite error rather than a false success: {msg:?}"
    );
}

/// Renaming an endpoint rewrites the sequence steps that referenced it (closing
/// the latent rename-breaks-sequences bug).
#[test]
fn rename_endpoint_rewrites_referencing_steps() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = workspace_fixture(dir.path());
    let root = dir.path();
    std::fs::create_dir_all(root.join("sequences")).unwrap();
    std::fs::write(
        root.join("sequences/flow.toml"),
        "name = \"flow\"\n\n[[step]]\nseq = 0\nendpoint = \"users/get.toml\"\n",
    )
    .unwrap();
    // Select the endpoint under `users`, then rename it.
    app.explorer.expand().unwrap();
    app.explorer.cursor = 1;
    assert_eq!(app.explorer.selected_kind(), Some(RowKind::Endpoint));
    app.commit_rename("fetched".to_owned()).unwrap();
    let seq = persistence::load_sequence(&root.join("sequences/flow.toml")).unwrap();
    assert_eq!(
        seq.steps[0].endpoint, "users/fetched.toml",
        "referencing step repointed by the rename"
    );
}

// ---- per-endpoint insecure-TLS opt-in (M8.1 Item 1) ------------------

/// Seeds a one-endpoint workspace on disk and loads that endpoint into a buffer,
/// returning `(app, endpoint_file_path)`.
fn app_with_loaded_endpoint(root: &Path) -> (App, PathBuf) {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = root.join("api");
    std::fs::create_dir(&coll).unwrap();
    let file = coll.join("get.toml");
    std::fs::write(
        &file,
        "seq = 0\nname = \"Get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/x\"\n",
    )
    .unwrap();
    let ws = open_workspace(root).unwrap();
    let mut app = App::new(ws, KeyMap::default()).unwrap();
    let endpoint = churl_core::persistence::load_endpoint(&file).unwrap();
    app.open_or_focus_buffer(SelectedEndpoint {
        display_path: "api/Get".to_owned(),
        file: file.clone(),
        collection: 0,
        endpoint,
    });
    (app, file)
}

#[test]
fn toggle_endpoint_insecure_persists_and_drives_effective_badge() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, file) = app_with_loaded_endpoint(dir.path());

    // Starts secure: neither session nor endpoint opted in.
    assert!(!app.selected().unwrap().endpoint.request.insecure);
    assert!(!app.insecure_active(), "badge off while fully secure");

    // Toggle on: in-memory flag, effective badge, and the on-disk file all flip.
    app.toggle_endpoint_insecure();
    assert!(app.selected().unwrap().endpoint.request.insecure);
    assert!(
        app.insecure_active(),
        "effective badge reflects the per-endpoint opt-in even with a secure session"
    );
    assert!(
        churl_core::persistence::load_endpoint(&file)
            .unwrap()
            .request
            .insecure,
        "the opt-in persisted to disk"
    );

    // Toggle off: flag, badge, and disk return to secure.
    app.toggle_endpoint_insecure();
    assert!(!app.selected().unwrap().endpoint.request.insecure);
    assert!(!app.insecure_active());
    assert!(
        !churl_core::persistence::load_endpoint(&file)
            .unwrap()
            .request
            .insecure
    );
}

#[test]
fn toggle_endpoint_insecure_without_selection_is_a_noop_message() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.toggle_endpoint_insecure();
    assert_eq!(
        app.message.as_ref().map(|m| m.text.as_str()),
        Some("no endpoint selected")
    );
}

#[test]
fn client_for_builds_insecure_variant_only_when_the_endpoint_diverges() {
    // White-box: with a secure session, a secure endpoint reuses the shared
    // client (no variant built) while an opted-in endpoint builds and caches a
    // distinct insecure client. A client rebuild invalidates the cache.
    let mut app = App::new(None, KeyMap::default()).unwrap();
    app.client = Some(churl_core::http::build_client_with(&Default::default()).unwrap());

    let mut secure = open_bare_request();
    secure.insecure = false;
    let mut opted = open_bare_request();
    opted.insecure = true;

    assert!(app.client_for(&secure).is_some());
    assert!(
        app.insecure_client.is_none(),
        "a secure endpoint must not build the insecure variant"
    );

    assert!(app.client_for(&opted).is_some());
    assert!(
        app.insecure_client.is_some(),
        "an opted-in endpoint builds and caches the insecure variant"
    );

    app.rebuild_client().unwrap();
    assert!(
        app.insecure_client.is_none(),
        "rebuild_client invalidates the cached insecure variant"
    );
}

/// A bare GET request used by the client_for divergence test.
fn open_bare_request() -> Request {
    Request {
        method: churl_core::model::Method::Get,
        url: "https://api.test/x".to_owned(),
        headers: Vec::new(),
        params: Vec::new(),
        body: None,
        auth: None,
        insecure: false,
    }
}
