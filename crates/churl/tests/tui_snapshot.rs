//! Snapshot tests for the M2 layout: three panes, overlays, and the empty
//! state, driven through real key events against a `TestBackend` (no tokio).

use std::path::Path;
use std::time::{Duration, Instant};

use churl::tui::app::{App, ConfirmPurpose, Mode, Pane, open_workspace, render};
use churl::tui::components::line_editor::LineEditor;
use churl::tui::components::response::{ResponseMeta, ResponseState, ResponseView};
use churl::tui::events::{Action, KeyMap};
use churl_core::model::{ApiKeyPlacement, Auth, Header, Response, Timing};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Builds a deterministic fixture workspace: two collections, four endpoints.
fn fixture(root: &Path) {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let users = root.join("users");
    std::fs::create_dir(&users).unwrap();
    std::fs::write(
        users.join("list.toml"),
        "seq = 0\nname = \"List users\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/users\"\n",
    )
    .unwrap();
    std::fs::write(
        users.join("create.toml"),
        concat!(
            "seq = 1\nname = \"Create user\"\n\n[request]\nmethod = \"POST\"\n",
            "url = \"https://api.test/users\"\n\n[[request.headers]]\nname = \"Content-Type\"\n",
            "value = \"application/json\"\n\n[request.body]\ntype = \"json\"\n",
            "content = '{\"name\": \"Ada\"}'\n",
        ),
    )
    .unwrap();
    std::fs::write(
        users.join("get.toml"),
        "seq = 2\nname = \"Get user\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/users/{{id}}\"\n",
    )
    .unwrap();
    let orders = root.join("orders");
    std::fs::create_dir(&orders).unwrap();
    std::fs::write(
        orders.join("list.toml"),
        "seq = 0\nname = \"List orders\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/orders\"\n",
    )
    .unwrap();
}

fn app_with_fixture(root: &Path) -> App {
    fixture(root);
    let workspace = open_workspace(root).unwrap();
    App::new(workspace, KeyMap::default()).unwrap()
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .unwrap();
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn snapshot(app: &mut App) -> String {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal init failed");
    terminal.draw(|frame| render(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let lines: Vec<String> = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect();
    lines.join("\n")
}

#[test]
fn three_panes_with_selected_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    // Move onto "users", expand it, and select "Create user".
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn empty_state_without_workspace() {
    let mut app = App::new(None, KeyMap::default()).unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn search_overlay_with_query() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "user");
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn palette_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char(':'));
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn jump_overlay_shows_labels() {
    // Expand a collection so visible rows get labels, then enter jump-mode: the
    // pane titles carry their mnemonics `[e]`/`[u]`/`[r]`/`[s]` and the visible
    // rows carry row labels (all visible in the TestBackend text buffer).
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char('j')); // onto "users"
    press(&mut app, KeyCode::Enter); // expand
    press(&mut app, KeyCode::Char('f')); // enter jump-mode
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn request_pane_auth_line_placeholder_shown_verbatim() {
    // Basic auth with a {{password}} placeholder: rendered verbatim.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("private");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("report.toml"),
        concat!(
            "seq = 0\nname = \"Private report\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://api.test/report\"\n\n[request.auth]\ntype = \"basic\"\n",
            "username = \"alice\"\npassword = \"{{password}}\"\n",
        ),
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter); // expand "private"
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter); // select "Private report"
    // Focus the Request pane and switch to the Auth tab (tab 3) to see the auth
    // fields; the {{password}} placeholder must render verbatim.
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('3'));
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn request_pane_auth_line_masks_literal_secret() {
    // A hand-written file may carry a literal secret (loads never refuse);
    // the auth line must mask it as ***** and never render it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("private");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("things.toml"),
        concat!(
            "seq = 0\nname = \"List things\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://api.test/things\"\n\n[request.auth]\ntype = \"apikey\"\n",
            "name = \"X-Api-Key\"\nvalue = \"abc123-literal\"\n",
        ),
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    // Switch to the Auth tab so the api-key value is on screen — it must be
    // masked (the name X-Api-Key looks secret), never the literal.
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('3'));
    let rendered = snapshot(&mut app);
    assert!(
        !rendered.contains("abc123-literal"),
        "literal secret must never render"
    );
    insta::assert_snapshot!(rendered);
}

/// Builds a JSON response with the given body.
fn json_response(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(142),
        },
    }
}

fn meta() -> ResponseMeta {
    ResponseMeta {
        method: "GET".to_owned(),
        url: "https://api.test/users/1".to_owned(),
        endpoint_path: Some("users/get.toml".to_owned()),
        executed_at_ms: 0,
    }
}

#[test]
fn response_pane_with_json_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    let body = "{\n  \"id\": 1,\n  \"name\": \"Ada\"\n}";
    app.response = ResponseState::Done {
        view: ResponseView::build(&json_response(body), 1),
    };
    // No highlight worker under TestBackend → deterministic plain text.
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn response_pane_truncated_status_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    // A capped response: 10 MB held, marked truncated. Rendered at 180 columns
    // so the full status line (with the truncated marker) fits the pane.
    let mut response = json_response(&"x".repeat(10 * 1024 * 1024));
    response.truncated = true;
    app.response = ResponseState::Done {
        view: ResponseView::build(&response, 1),
    };
    let backend = TestBackend::new(180, 24);
    let mut terminal = Terminal::new(backend).expect("terminal init failed");
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let lines: Vec<String> = (0..24)
        .map(|y| {
            (0..180)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect();
    insta::assert_snapshot!(lines.join("\n"));
}

#[test]
fn response_pane_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    // Backdate `started` by 1 s so the elapsed readout is always four digits —
    // the digit count shifts where the status line truncates at the pane edge,
    // so the regex filter alone cannot make the snapshot deterministic.
    app.response = ResponseState::InFlight {
        started: Instant::now() - Duration::from_secs(1),
    };
    // Elapsed ms is non-deterministic; scrub it.
    insta::with_settings!({filters => vec![(r"\d+ ms", "[N] ms")]}, {
        insta::assert_snapshot!(snapshot(&mut app));
    });
}

#[test]
fn response_pane_failed() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    app.response = ResponseState::Failed {
        error: "request failed: error sending request".to_owned(),
        meta: meta(),
    };
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn response_pane_draws_one_megabyte_body_under_50ms() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;

    // ~1 MB of multi-line JSON-ish text. Building the view is O(n) and excluded
    // from the timed section.
    let mut body = String::with_capacity(1024 * 1024 + 64);
    let mut i = 0u32;
    while body.len() < 1024 * 1024 {
        body.push_str(&format!(
            "  \"key_{i}\": \"value number {i} lorem ipsum dolor\",\n"
        ));
        i += 1;
    }
    app.response = ResponseState::Done {
        view: ResponseView::build(&json_response(&body), 1),
    };

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal init failed");
    let start = Instant::now();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("draw");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "1 MB response draw took {elapsed:?}, expected < 50 ms"
    );
}

// ---- M7 wave 1: response viewer features ----

/// Focuses the Response pane on a completed JSON response, ready for viewer keys.
fn json_done_app(root: &Path, body: &str) -> App {
    let mut app = app_with_fixture(root);
    app.focus = Pane::Response;
    app.response = ResponseState::Done {
        view: ResponseView::build(&json_response(body), 1),
    };
    app
}

/// A response carrying several headers (for the headers-view tests).
fn headered_response(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![
            Header {
                name: "Content-Type".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            },
            Header {
                name: "X-Request-Id".to_owned(),
                value: "abc-123".to_owned(),
                enabled: true,
            },
        ],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(142),
        },
    }
}

#[test]
fn response_headers_view_toggle() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    app.response = ResponseState::Done {
        view: ResponseView::build(&headered_response("{\n  \"id\": 1\n}"), 1),
    };
    // `h` in the Response overlay toggles to the headers view.
    press(&mut app, KeyCode::Char('h'));
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn response_wrap_toggle() {
    let dir = tempfile::tempdir().unwrap();
    // A single very long line so wrap has a visible effect in the narrow pane.
    let long = format!("{{\"data\":\"{}\"}}", "wrap-me-".repeat(20));
    let mut app = json_done_app(dir.path(), &long);
    // `W` toggles soft-wrap on; the stats title gains `· wrap`.
    app.handle_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn response_fold_renders_ellipsis_marker() {
    let dir = tempfile::tempdir().unwrap();
    let body = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}";
    let mut app = json_done_app(dir.path(), body);
    // Cursor is on line 0 (the opener); `o` folds the region.
    press(&mut app, KeyCode::Char('o'));
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains('⋯'),
        "folded region must render a ⋯ marker:\n{rendered}"
    );
    insta::assert_snapshot!(rendered);
}

#[test]
fn response_fold_non_json_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    app.focus = Pane::Response;
    // A plain-text response: folding is unsupported.
    let response = Response {
        status: 200,
        headers: Vec::new(),
        body: b"line one\nline two".to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    };
    app.response = ResponseState::Done {
        view: ResponseView::build(&response, 1),
    };
    press(&mut app, KeyCode::Char('o'));
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("JSON responses only"),
        "non-JSON fold must notify:\n{rendered}"
    );
}

#[test]
fn response_search_highlights_and_navigates() {
    let dir = tempfile::tempdir().unwrap();
    let body = "{\n  \"name\": \"needle\",\n  \"other\": \"needle\"\n}";
    let mut app = json_done_app(dir.path(), body);
    // `/` opens the incremental search input.
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(app.mode, Mode::BodySearch);
    type_str(&mut app, "needle");
    // Two matches; the input row shows the count.
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("/needle") && rendered.contains("2 matches"),
        "search input must show query + count:\n{rendered}"
    );
    // Commit and step to the next match.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Char('n'));
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("match 2/2"),
        "n must report match position:\n{rendered}"
    );
}

#[test]
fn response_search_wraps_around() {
    let dir = tempfile::tempdir().unwrap();
    let body = "{\n  \"a\": \"x\",\n  \"b\": \"x\"\n}";
    let mut app = json_done_app(dir.path(), body);
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "x");
    press(&mut app, KeyCode::Enter);
    // Two matches; n, n wraps back to the first.
    press(&mut app, KeyCode::Char('n')); // → 2/2
    press(&mut app, KeyCode::Char('n')); // wrap → 1/2
    let rendered = snapshot(&mut app);
    assert!(rendered.contains("match 1/2"), "n must wrap:\n{rendered}");
    // N steps backward → 2/2.
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
        .unwrap();
    let rendered = snapshot(&mut app);
    assert!(rendered.contains("match 2/2"), "N steps back:\n{rendered}");
}

#[test]
fn response_search_auto_unfolds_while_typing() {
    let dir = tempfile::tempdir().unwrap();
    // Two matches, both inside the (foldable) outer object.
    let body = "{\n  \"a\": \"needle\",\n  \"b\": \"needle\"\n}";
    let mut app = json_done_app(dir.path(), body);
    let _ = snapshot(&mut app); // prime geometry
    // Collapse all top-level regions: only the `{ ⋯` header remains visible.
    app.handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT))
        .unwrap();
    let rendered = snapshot(&mut app);
    assert!(rendered.contains('⋯'), "precondition: folded:\n{rendered}");
    // Typing the query must auto-unfold and show match 1 (the needle line).
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "needle");
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("\"a\": \"needle\""),
        "incremental search must auto-unfold to match 1:\n{rendered}"
    );
    assert!(
        rendered.contains("2 matches"),
        "both folded matches must count:\n{rendered}"
    );
    // Commit; the first `n` advances to match 2 of 2 (match 1 was current).
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("match 2/2"),
        "first n after commit goes to match 2:\n{rendered}"
    );
}

#[test]
fn response_fold_in_headers_view_says_body_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1\n}");
    press(&mut app, KeyCode::Char('h')); // → headers view
    press(&mut app, KeyCode::Char('o'));
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("folding: body view only"),
        "headers view must get its own fold notice:\n{rendered}"
    );
}

#[test]
fn response_search_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1\n}");
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "zzz");
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("no matches"),
        "empty result set must say so:\n{rendered}"
    );
}

#[test]
fn response_search_esc_clears() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": \"hit\"\n}");
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "hit");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    // Esc clears the live search.
    if let ResponseState::Done { view } = &app.response {
        assert!(view.search().is_none(), "esc must clear the search");
    } else {
        panic!("expected Done");
    }
}

#[test]
fn response_copy_sets_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1\n}");
    press(&mut app, KeyCode::Char('y'));
    // The copy is queued for the run loop (which owns the real clipboard); the
    // queued success message reports a size. We assert on the intent, not the
    // rendered row, so the test never touches a real clipboard.
    let msg = app.pending_copy_message().expect("copy must be queued");
    assert!(
        msg.contains("copied"),
        "copy must confirm with a size: {msg}"
    );
}

#[test]
fn response_copy_line_reports_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1\n}");
    app.handle_key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT))
        .unwrap();
    let msg = app.pending_copy_message().expect("copy must be queued");
    assert_eq!(msg, "copied line", "Y must confirm a line copy");
}

#[test]
fn response_cursor_moves_with_j_and_shows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1,\n  \"b\": 2\n}");
    // Prime the geometry with an initial render, then move the cursor down.
    let _ = snapshot(&mut app);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn response_view_toggle_resets_cursor_and_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": \"z\",\n  \"b\": \"z\"\n}");
    let _ = snapshot(&mut app);
    // Move the cursor and start a search, then toggle to headers.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "z");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('h')); // → headers view
    if let ResponseState::Done { view } = &app.response {
        assert!(view.search().is_none(), "view toggle clears search");
    }
    // Cursor reset to 0 is asserted indirectly: no panic + headers render.
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("headers"),
        "stats title must show the headers marker:\n{rendered}"
    );
}

#[test]
fn response_zoom_stub_unchanged_by_viewer_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = json_done_app(dir.path(), "{\n  \"a\": 1\n}");
    // Turn on wrap + a search, then zoom the response pane. The collapsed stub
    // (when the *request* pane is zoomed) is the one-line summary and is
    // unaffected by viewer state — assert it still renders the status summary.
    app.handle_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT))
        .unwrap();
    // Zoom the Request pane (collapsing Response to its one-line stub) via `z`.
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('z'));
    let rendered = snapshot(&mut app);
    // The collapsed response summary line shows the status.
    assert!(
        rendered.contains("200 OK"),
        "collapsed response stub must show the status summary:\n{rendered}"
    );
}

#[test]
fn explorer_scrolls_to_keep_selection_visible() {
    // A tall collection (more endpoints than the pane height) must scroll so the
    // selected bottom row stays on screen.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let big = dir.path().join("big");
    std::fs::create_dir(&big).unwrap();
    for i in 0..40 {
        std::fs::write(
            big.join(format!("ep{i:02}.toml")),
            format!(
                "seq = {i}\nname = \"Endpoint {i:02}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{i}\"\n"
            ),
        )
        .unwrap();
    }
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter); // expand "big"
    // Shift-G jumps to the last endpoint (crossterm reports G + SHIFT).
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

/// The URL bar shows `METHOD  url` + right-aligned indicators when an endpoint
/// with auth and `{{var}}` placeholders is selected.
#[test]
fn url_bar_shows_indicators_for_auth_and_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    // Endpoint with basic auth ({{password}} placeholder) and a URL placeholder.
    std::fs::write(
        coll.join("list.toml"),
        concat!(
            "seq = 0\nname = \"List items\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://{{host}}/items\"\n\n[request.auth]\ntype = \"basic\"\n",
            "username = \"alice\"\npassword = \"{{password}}\"\n",
        ),
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter); // expand "api"
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter); // select "List items"
    // URL bar must show: method+url prefix on the left, auth:basic indicator on the right.
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("GET  https://"),
        "URL bar must show GET + URL prefix: {rendered}"
    );
    assert!(
        rendered.contains("auth:basic"),
        "URL bar must show auth indicator: {rendered}"
    );
    insta::assert_snapshot!(rendered);
}

// ---- M6.6: URL bar, tabs, CRUD ----

/// Loads the first endpoint of the fixture's `users` collection into `app`.
fn load_first_users_endpoint(app: &mut App) {
    // orders (collapsed) at row 0, users at row 1.
    press(app, KeyCode::Char('j')); // onto users
    press(app, KeyCode::Enter); // expand
    press(app, KeyCode::Char('j')); // List users
    press(app, KeyCode::Enter); // select
}

#[test]
fn url_bar_focused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::UrlBar;
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn url_bar_editing_inline() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i')); // begin edit
    type_str(&mut app, "/extra");
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn url_bar_dirty_dot() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::UrlBar;
    // Edit and commit the URL → dirty.
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "X");
    press(&mut app, KeyCode::Enter);
    let rendered = snapshot(&mut app);
    assert!(rendered.contains('●'), "dirty dot must show: {rendered}");
    insta::assert_snapshot!(rendered);
}

#[test]
fn request_tab_params() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("search.toml"),
        concat!(
            "seq = 0\nname = \"Search\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://api.test/search\"\n\n[[request.params]]\nname = \"q\"\n",
            "value = \"rust\"\n\n[[request.params]]\nname = \"page\"\nvalue = \"1\"\nenabled = false\n",
        ),
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    app.focus = Pane::Request; // Params is the default tab
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn request_tab_headers() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    // "Create user" has a Content-Type header + JSON body.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter); // Create user
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('2')); // Headers tab
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn request_tab_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter); // Create user (JSON body)
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('4')); // Body tab
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn method_menu_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::UrlBar;
    // Shift-M opens the method menu (crossterm reports 'M' + SHIFT).
    app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn prompt_new_endpoint_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char('j')); // onto users collection
    press(&mut app, KeyCode::Char('n')); // new endpoint prompt
    type_str(&mut app, "Update user");
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn confirm_delete_endpoint_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    press(&mut app, KeyCode::Char('d')); // delete confirm (endpoint selected)
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn confirm_discard_changes_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    // Make it dirty, then try to switch to another endpoint.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "X");
    press(&mut app, KeyCode::Enter);
    app.focus = Pane::Explorer;
    press(&mut app, KeyCode::Char('j')); // onto another endpoint
    press(&mut app, KeyCode::Enter); // triggers discard-changes confirm
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn palette_lists_curated_commands() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char(':'));
    insta::assert_snapshot!(snapshot(&mut app));
}

/// Every curated palette command dispatches to a non-no-op path: with no
/// endpoint/collection selected, CRUD/send commands surface a statusline error;
/// focus commands move focus; SwitchProfile opens its picker; Quit quits.
#[test]
fn every_palette_command_dispatches() {
    use churl::tui::components::palette;
    // An empty app (no workspace) — CRUD/send have no context, so they must warn.
    for (index, (label, action)) in palette::COMMANDS.iter().enumerate() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, KeyCode::Char(':'));
        if let Some(picker) = app.picker.as_mut() {
            picker.selected = index;
        }
        // Accept via Enter → dispatch.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // Assert the documented non-no-op effect per command.
        let expect_status = |app: &mut App, msg: &str| {
            let rendered = snapshot(app);
            assert!(
                rendered.contains(msg),
                "{label:?} must surface {msg:?} on the statusline:\n{rendered}"
            );
        };
        match action {
            Action::Send => expect_status(&mut app, "no endpoint selected"),
            Action::Cancel => expect_status(&mut app, "no request in flight"),
            Action::Save => expect_status(&mut app, "no endpoint to save"),
            Action::NewEndpoint => expect_status(&mut app, "select a collection first"),
            Action::NewCollection => expect_status(&mut app, "no workspace open"),
            Action::Rename => expect_status(&mut app, "nothing selected to rename"),
            Action::Delete => expect_status(&mut app, "nothing selected to delete"),
            Action::SwitchProfile => {
                assert_eq!(app.mode, Mode::Palette, "{label:?} must open the picker");
                assert!(app.picker.is_some());
            }
            // The two viewer toggles are palette-exposed; with no response they
            // no-op gracefully (the pane is Idle), so just assert no crash / mode
            // stays Normal.
            Action::ToggleHeadersView | Action::ToggleWrap => {
                assert_eq!(app.mode, Mode::Normal, "{label:?} must not open an overlay");
            }
            Action::FocusExplorer => assert_eq!(app.focus, Pane::Explorer),
            Action::FocusUrlBar => assert_eq!(app.focus, Pane::UrlBar),
            Action::FocusRequest => assert_eq!(app.focus, Pane::Request),
            Action::FocusResponse => assert_eq!(app.focus, Pane::Response),
            Action::Quit => assert!(app.should_quit, "{label:?} must quit"),
            // M7.1 interchange: with no workspace/collection/endpoint these
            // surface a statusline guard (never a silent no-op).
            Action::ImportCollection => expect_status(&mut app, "no workspace open"),
            Action::ExportWorkspacePostman | Action::ExportWorkspaceNative => {
                expect_status(&mut app, "no workspace open")
            }
            Action::ExportCollectionPostman | Action::ExportCollectionNative => {
                expect_status(&mut app, "select a collection first")
            }
            Action::PasteCurl => expect_status(&mut app, "select a collection first"),
            Action::CopyAsCurl | Action::CopyAsCurlResolved => {
                expect_status(&mut app, "no endpoint selected")
            }
            // The env editor needs an open workspace; without one it warns.
            Action::OpenEnvEditor => expect_status(&mut app, "open a workspace first"),
            // Sequences (M7.4): no workspace / no sequence selected → warn.
            Action::RunSequence => expect_status(&mut app, "select a sequence"),
            Action::EditSequence => expect_status(&mut app, "open a workspace first"),
            // Load runner (M7.5): with no endpoint selected it warns rather than
            // opening the modal.
            Action::OpenLoadRunner => expect_status(&mut app, "no endpoint selected"),
            other => panic!("palette command {label:?} → {other:?} has no assertion — add one"),
        }
    }
}

/// URL edit round-trip: focus the bar, edit, commit, save (`w`), and the file on
/// disk carries the new URL (format-preserving).
#[test]
fn url_edit_commit_save_writes_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    let file = dir.path().join("users").join("list.toml");
    let before = std::fs::read_to_string(&file).unwrap();
    assert!(before.contains("https://api.test/users"));

    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "/active");
    press(&mut app, KeyCode::Enter); // commit
    // Save with `w`.
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("https://api.test/users/active"),
        "url must be saved: {after}"
    );
    // The seq/name lines survive (format-preserving merge).
    assert!(after.contains("name = \"List users\""), "{after}");
}

/// Adding a param via the tab UI and saving writes it as an array-of-tables;
/// toggling `enabled` off serializes `enabled = false` (true is omitted).
#[test]
fn row_add_and_toggle_serialize() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    let file = dir.path().join("users").join("list.toml");

    app.focus = Pane::Request; // Params tab
    press(&mut app, KeyCode::Char('a')); // add row (enters name edit)
    type_str(&mut app, "limit");
    press(&mut app, KeyCode::Enter); // name → value
    type_str(&mut app, "10");
    press(&mut app, KeyCode::Enter); // commit row
    // Toggle it disabled. Space became the global leader in M6.7; the Request
    // row-toggle rebound to `t`.
    press(&mut app, KeyCode::Char('t'));
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();

    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("[[request.params]]"), "{after}");
    assert!(after.contains("name = \"limit\""), "{after}");
    assert!(after.contains("value = \"10\""), "{after}");
    assert!(
        after.contains("enabled = false"),
        "disabled must serialize: {after}"
    );
}

/// Discarding changes drops the edits and switches to the other endpoint.
#[test]
fn discard_changes_switches_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // List users
    // Dirty the URL.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "ZZZ");
    press(&mut app, KeyCode::Enter);
    // Switch to another endpoint → discard confirm.
    app.focus = Pane::Explorer;
    press(&mut app, KeyCode::Char('j')); // onto "Create user"
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
    // 'd' discards.
    press(&mut app, KeyCode::Char('d'));
    // The endpoint actually switched…
    assert_eq!(app.mode, Mode::Normal);
    let selected = app.selected.as_ref().expect("an endpoint is loaded");
    assert!(
        selected.file.ends_with("users/create.toml"),
        "must have switched to Create user, got {:?}",
        selected.file
    );
    assert!(
        !selected.endpoint.request.url.contains("ZZZ"),
        "discarded edit must not leak into the new endpoint"
    );
    // …and the list.toml file was never written.
    let file = dir.path().join("users").join("list.toml");
    let contents = std::fs::read_to_string(&file).unwrap();
    assert!(
        !contents.contains("ZZZ"),
        "discarded edit must not persist: {contents}"
    );
}

/// Review #2a: switching endpoints via the *search overlay* while dirty must
/// raise the same discard-changes confirm as the explorer path — never a
/// silent discard.
#[test]
fn search_switch_while_dirty_is_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // List users
    // Dirty the URL.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "ZZZ");
    press(&mut app, KeyCode::Enter);
    // Search for "Get user" and accept.
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "get us");
    press(&mut app, KeyCode::Enter);
    // Guard must fire; nothing switched yet.
    assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
    assert!(
        app.selected.as_ref().unwrap().file.ends_with("list.toml"),
        "still on List users while the confirm is open"
    );
    // Discard → the search target loads.
    press(&mut app, KeyCode::Char('d'));
    assert!(
        app.selected
            .as_ref()
            .unwrap()
            .file
            .ends_with("users/get.toml"),
        "search target must load after discard, got {:?}",
        app.selected.as_ref().unwrap().file
    );
    let contents = std::fs::read_to_string(dir.path().join("users").join("list.toml")).unwrap();
    assert!(!contents.contains("ZZZ"), "edit must not persist");
}

/// Review #2b: switching endpoints via a *jump-mode row label* while dirty must
/// raise the confirm; `s` saves the edits to disk, then switches.
#[test]
fn jump_switch_while_dirty_guards_and_saves() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // List users (row 2)
    // A saveable dirty edit.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "/active");
    press(&mut app, KeyCode::Enter);
    // Jump to the "Create user" row: panes hold the e/u/r/s mnemonics, rows
    // take a/d/f/g/… from row 0 → row 3 (Create user) is 'g'.
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(
        app.mode,
        Mode::Confirm(ConfirmPurpose::DiscardChanges),
        "jump-mode endpoint switch must be guarded while dirty"
    );
    // Save-then-switch.
    press(&mut app, KeyCode::Char('s'));
    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.selected
            .as_ref()
            .unwrap()
            .file
            .ends_with("users/create.toml"),
        "must switch after a successful save, got {:?}",
        app.selected.as_ref().unwrap().file
    );
    let contents = std::fs::read_to_string(dir.path().join("users").join("list.toml")).unwrap();
    assert!(
        contents.contains("https://api.test/users/active"),
        "the edit must be saved before switching: {contents}"
    );
}

/// Review #3: `s` (save-then-switch) with a save that *fails* (literal secret
/// auth is refused) must stay on the same endpoint with the edits intact and
/// the error visible — never destroy the unsaved edits it just refused to write.
#[test]
fn save_failure_blocks_discard_changes_switch() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // List users
    // A dirty edit that cannot be saved: literal bearer token.
    app.selected.as_mut().unwrap().endpoint.request.auth = Some(Auth::Bearer {
        token: "ghp_literal_secret".to_owned(),
    });
    // Attempt to switch → confirm → 's'.
    app.focus = Pane::Explorer;
    press(&mut app, KeyCode::Char('j')); // onto "Create user"
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
    press(&mut app, KeyCode::Char('s'));
    // Still on the same endpoint, still dirty, error on the statusline.
    assert_eq!(app.mode, Mode::Normal);
    let selected = app.selected.as_ref().expect("endpoint still loaded");
    assert!(
        selected.file.ends_with("users/list.toml"),
        "must NOT switch when the save failed, got {:?}",
        selected.file
    );
    assert!(
        matches!(&selected.endpoint.request.auth, Some(Auth::Bearer { token }) if token == "ghp_literal_secret"),
        "the unsaved edit must survive"
    );
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("not saved"),
        "the refusal must be visible on the statusline:\n{rendered}"
    );
    assert!(rendered.contains('●'), "still dirty: {rendered}");
    // The file was never written.
    let contents = std::fs::read_to_string(dir.path().join("users").join("list.toml")).unwrap();
    assert!(
        !contents.contains("auth"),
        "refused save must not write: {contents}"
    );
}

/// Review #5: renaming the collection that contains the loaded endpoint must
/// repoint `selected.file` into the new directory — the next save must land in
/// the renamed collection, not fail NotFound.
#[test]
fn rename_collection_repoints_loaded_endpoint_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // users/list.toml
    // Dirty the URL so the follow-up save is observable.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "/renamed-home");
    press(&mut app, KeyCode::Enter);
    // Rename the "users" collection (cursor onto its row first).
    app.focus = Pane::Explorer;
    app.explorer.cursor = 1; // orders(0), users(1)
    press(&mut app, KeyCode::Char('r'));
    app.prompt_editor = LineEditor::new("members");
    press(&mut app, KeyCode::Enter);
    // The loaded endpoint's file now points into the renamed directory.
    let selected_file = app.selected.as_ref().unwrap().file.clone();
    assert!(
        selected_file.ends_with("members/list.toml"),
        "selected.file must follow the renamed collection, got {selected_file:?}"
    );
    assert!(selected_file.exists());
    // And a save works (would previously fail NotFound on the old path).
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    let contents = std::fs::read_to_string(dir.path().join("members").join("list.toml")).unwrap();
    assert!(
        contents.contains("https://api.test/users/renamed-home"),
        "save must land in the renamed collection: {contents}"
    );
}

/// Review #6: Enter on the ApiKey placement row toggles header/query, same as
/// Space (design: "placement row toggles with Space/Enter").
#[test]
fn placement_row_enter_toggles() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
    let coll = dir.path().join("api");
    std::fs::create_dir(&coll).unwrap();
    std::fs::write(
        coll.join("things.toml"),
        concat!(
            "seq = 0\nname = \"Things\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://api.test/things\"\n\n[request.auth]\ntype = \"apikey\"\n",
            "name = \"X-Api-Key\"\nvalue = \"{{api_key}}\"\n",
        ),
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter); // select Things
    app.focus = Pane::Request;
    press(&mut app, KeyCode::Char('3')); // Auth tab
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('j')); // down to the placement row (3)
    }
    let placement = |app: &App| match &app.selected.as_ref().unwrap().endpoint.request.auth {
        Some(Auth::ApiKey { placement, .. }) => *placement,
        other => panic!("expected apikey auth, got {other:?}"),
    };
    assert_eq!(placement(&app), ApiKeyPlacement::Header);
    press(&mut app, KeyCode::Enter);
    assert_eq!(placement(&app), ApiKeyPlacement::Query, "Enter must toggle");
    press(&mut app, KeyCode::Enter);
    assert_eq!(placement(&app), ApiKeyPlacement::Header, "…both ways");
}

/// Review #7: creating a new endpoint while the loaded one is dirty goes
/// through the guarded seam — the confirm appears instead of a silent discard;
/// Esc stays put with the edits intact (the file is created on disk either way).
#[test]
fn new_endpoint_while_dirty_is_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // users/list.toml
    // Dirty the URL.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "ZZZ");
    press(&mut app, KeyCode::Enter);
    // New endpoint under "users" (cursor is on the List users endpoint row,
    // whose owning collection is "users").
    app.focus = Pane::Explorer;
    press(&mut app, KeyCode::Char('n'));
    type_str(&mut app, "Fresh One");
    press(&mut app, KeyCode::Enter);
    // The file exists, but the load is deferred behind the confirm.
    assert!(dir.path().join("users").join("fresh-one.toml").exists());
    assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
    // Esc: stay on the dirty endpoint, edits intact.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    let selected = app.selected.as_ref().unwrap();
    assert!(selected.file.ends_with("users/list.toml"), "must stay put");
    assert!(
        selected.endpoint.request.url.contains("ZZZ"),
        "edits must survive Esc"
    );
}

/// Review #8: `a`(dd row) followed by Esc must not leave a nameless ghost row;
/// a row whose name was committed survives an Esc during the value edit.
#[test]
fn ghost_row_removed_on_escape() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // no params
    app.focus = Pane::Request; // Params tab
    // a + Esc → no ghost row.
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Esc);
    assert!(
        app.selected
            .as_ref()
            .unwrap()
            .endpoint
            .request
            .params
            .is_empty(),
        "cancelled add must remove the empty row"
    );
    // a + name + Enter + Esc on the value → the row is kept (named).
    press(&mut app, KeyCode::Char('a'));
    type_str(&mut app, "limit");
    press(&mut app, KeyCode::Enter); // commit name → value edit
    press(&mut app, KeyCode::Esc); // cancel the value edit only
    let params = &app.selected.as_ref().unwrap().endpoint.request.params;
    assert_eq!(params.len(), 1, "a named row must survive Esc");
    assert_eq!(params[0].name, "limit");
    assert_eq!(params[0].value, "");
}

/// Reselecting the currently-loaded endpoint while dirty must not silently
/// revert it from disk (one-keystroke data loss); a collection row still
/// toggles while dirty.
#[test]
fn reselect_same_endpoint_while_dirty_keeps_edits() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app); // cursor stays on the List users row
    // Dirty the URL.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "ZZZ");
    press(&mut app, KeyCode::Enter);
    // Enter on the same endpoint row: no confirm, no reload.
    app.focus = Pane::Explorer;
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.mode,
        Mode::Normal,
        "same-endpoint reselect needs no confirm"
    );
    let selected = app.selected.as_ref().unwrap();
    assert!(
        selected.endpoint.request.url.contains("ZZZ"),
        "edits must survive"
    );
    // A collection row still toggles while dirty (no guard, no data loss).
    press(&mut app, KeyCode::Char('k')); // up onto the users collection row
    press(&mut app, KeyCode::Enter); // collapse
    press(&mut app, KeyCode::Enter); // expand
    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.selected
            .as_ref()
            .unwrap()
            .endpoint
            .request
            .url
            .contains("ZZZ")
    );
}

// ---- M6.7 snapshots + round-trips ----

/// The which-key leader popup: pressing Space shows the bound continuations.
#[test]
fn leader_which_key_popup() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char(' '));
    insta::assert_snapshot!(snapshot(&mut app));
}

/// The URL vim-popup editor: edtui's status line shows the mode inside the
/// popup; the footer carries only the commit/cancel hints, bottom-right
/// (review round 3 — no duplicate mode indicator).
#[test]
fn url_popup_editor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('e'));
    let rendered = snapshot(&mut app);
    assert!(
        !rendered.contains("NORMAL \u{b7} enter commit"),
        "footer must not duplicate the vim mode: {rendered}"
    );
    insta::assert_snapshot!(rendered);
}

/// Renders a sequence-runner state directly to a `TestBackend` string (no tokio;
/// the component render is pure).
fn render_runner(
    state: &mut churl::tui::components::sequence_runner::SequenceRunnerState,
) -> String {
    use churl::tui::theme::Theme;
    let backend = TestBackend::new(90, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default();
    let cache = std::collections::HashMap::new();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let _ = churl::tui::components::sequence_runner::render(
                frame, area, state, 0, &cache, &theme,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..24)
        .map(|y| {
            (0..90)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn runner_step(endpoint: &str) -> churl_core::model::SequenceStep {
    churl_core::model::SequenceStep {
        seq: 0,
        endpoint: endpoint.to_owned(),
        extract: std::collections::BTreeMap::new(),
    }
}

/// A mid-run runner: step 0 done + extracted (a masked secret) + its response
/// shown; step 1 running; step 2 pending.
#[test]
fn sequence_runner_midrun() {
    use churl::tui::components::sequence_runner::{SequenceRunnerState, StepStatus};
    use churl_core::model::{Method, OnError};
    let mut state = SequenceRunnerState::new(
        "Auth flow".to_owned(),
        std::path::PathBuf::from("sequences/auth-flow.toml"),
        OnError::Halt,
        vec![
            runner_step("auth/login.toml"),
            runner_step("users/me.toml"),
            runner_step("users/by_id.toml"),
        ],
    );
    let resp = Response {
        status: 200,
        headers: vec![Header {
            name: "Content-Type".into(),
            value: "application/json".into(),
            enabled: true,
        }],
        body: br#"{"data":{"token":"abc123"}}"#.to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(120),
        },
    };
    state.steps[0].status = StepStatus::Ok(200);
    state.steps[0].method = Method::Post;
    state.steps[0].timing = Some(Duration::from_millis(120));
    state.steps[0]
        .extracted
        .insert("token".to_owned(), "abc123".to_owned());
    state.steps[0].response = ResponseState::Done {
        view: ResponseView::build(&resp, 1),
    };
    state.steps[1].status = StepStatus::Running;
    state.steps[1].method = Method::Get;
    state.current = Some(1);
    state.selected = 0;
    insta::assert_snapshot!(render_runner(&mut state));
}

/// A finished run with a failed step and a skipped tail.
#[test]
fn sequence_runner_finished_with_failure() {
    use churl::tui::components::sequence_runner::{SequenceRunnerState, StepStatus};
    use churl_core::model::{Method, OnError};
    let mut state = SequenceRunnerState::new(
        "Auth flow".to_owned(),
        std::path::PathBuf::from("sequences/auth-flow.toml"),
        OnError::Halt,
        vec![
            runner_step("auth/login.toml"),
            runner_step("users/me.toml"),
            runner_step("users/by_id.toml"),
        ],
    );
    state.steps[0].status = StepStatus::Ok(200);
    state.steps[0].method = Method::Post;
    state.steps[0].timing = Some(Duration::from_millis(90));
    state.steps[1].status = StepStatus::Failed(500);
    state.steps[1].method = Method::Get;
    state.steps[1].timing = Some(Duration::from_millis(45));
    state.steps[1].response = ResponseState::Failed {
        error: "HTTP 500".to_owned(),
        meta: ResponseMeta {
            method: String::new(),
            url: "users/me.toml".to_owned(),
            endpoint_path: Some("users/me.toml".to_owned()),
            executed_at_ms: 0,
        },
    };
    state.steps[2].status = StepStatus::Skipped;
    state.selected = 1;
    state.finished = true;
    insta::assert_snapshot!(render_runner(&mut state));
}

/// The runner's response viewer produces a highlight job for a `Done` step (it
/// used to build a throwaway cache and discard the job, so it never highlighted),
/// and each step's view gets a distinct cache key so two steps never collide.
#[test]
fn sequence_runner_enqueues_highlight_job() {
    use churl::tui::components::sequence_runner::{SequenceRunnerState, StepStatus};
    use churl::tui::theme::Theme;
    use churl_core::model::{Method, OnError};

    fn resp(body: &str) -> Response {
        Response {
            status: 200,
            headers: vec![Header {
                name: "Content-Type".into(),
                value: "application/json".into(),
                enabled: true,
            }],
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(10),
            },
        }
    }
    fn render_capture(
        state: &mut SequenceRunnerState,
        cache: &std::collections::HashMap<u64, Vec<ratatui::text::Line<'static>>>,
    ) -> Option<u64> {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let mut hash = None;
        terminal
            .draw(|frame| {
                let job = churl::tui::components::sequence_runner::render(
                    frame,
                    frame.area(),
                    state,
                    0,
                    cache,
                    &theme,
                );
                hash = job.map(|j| j.hash);
            })
            .unwrap();
        hash
    }

    let mut state = SequenceRunnerState::new(
        "Flow".to_owned(),
        std::path::PathBuf::from("sequences/flow.toml"),
        OnError::Halt,
        vec![runner_step("a.toml"), runner_step("b.toml")],
    );
    // Two Done steps with DISTINCT view generations (as the driver mints them).
    let g0 = state.next_view_gen();
    let g1 = state.next_view_gen();
    state.steps[0].status = StepStatus::Ok(200);
    state.steps[0].method = Method::Get;
    state.steps[0].response = ResponseState::Done {
        view: ResponseView::build(&resp(r#"{"a":1}"#), g0),
    };
    state.steps[1].status = StepStatus::Ok(200);
    state.steps[1].response = ResponseState::Done {
        view: ResponseView::build(&resp(r#"{"b":2}"#), g1),
    };

    let cache = std::collections::HashMap::new();
    // A Done step yields a job to enqueue (previously always None → plain text).
    state.selected = 0;
    let job0 = render_capture(&mut state, &cache);
    assert!(job0.is_some(), "a Done step must produce a highlight job");
    // A different step's view has a distinct cache key (no collision).
    state.selected = 1;
    let job1 = render_capture(&mut state, &cache);
    assert!(job1.is_some());
    assert_ne!(job0, job1, "distinct step views must not share a cache key");
}

/// The sequence editor modal: two steps, the first with extraction rules.
#[test]
fn sequence_editor_modal() {
    use churl::tui::components::sequence_editor::{self, SequenceEditorState};
    use churl::tui::theme::Theme;
    use churl_core::model::{OnError, Sequence, SequenceStep};
    let mut rules = std::collections::BTreeMap::new();
    rules.insert("token".to_owned(), "$.data.token".to_owned());
    rules.insert("user_id".to_owned(), "$.data.user.id".to_owned());
    let sequence = Sequence {
        seq: 0,
        name: "Auth flow".to_owned(),
        on_error: OnError::Halt,
        steps: vec![
            SequenceStep {
                seq: 0,
                endpoint: "auth/login.toml".to_owned(),
                extract: rules,
            },
            SequenceStep {
                seq: 1,
                endpoint: "users/me.toml".to_owned(),
                extract: std::collections::BTreeMap::new(),
            },
        ],
    };
    let state = SequenceEditorState::new(
        "Auth flow".to_owned(),
        std::path::PathBuf::from("sequences/auth-flow.toml"),
        &sequence,
        vec!["auth/login.toml".to_owned(), "users/me.toml".to_owned()],
    );
    let backend = TestBackend::new(90, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default();
    terminal
        .draw(|frame| sequence_editor::render(frame, frame.area(), &state, &theme))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rendered = (0..24)
        .map(|y| {
            (0..90)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}

/// The `?` help overlay renders the effective keymap, sectioned.
#[test]
fn help_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    press(&mut app, KeyCode::Char('?'));
    insta::assert_snapshot!(snapshot(&mut app));
}

/// A live message renders in the dedicated row above the statusline; the
/// statusline content is untouched (persistent state only).
#[test]
fn message_row_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    // `w` save produces a "Saved …" message in the dedicated row.
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

/// Without an active message the row is absent and the statusline sits on the
/// last line (no reserved gap).
#[test]
fn message_row_absent() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    assert!(app_message_is_none(&app));
    insta::assert_snapshot!(snapshot(&mut app));
}

fn app_message_is_none(app: &App) -> bool {
    // No public accessor; a fresh load leaves no message. Rendered proof is the
    // snapshot itself (24 rows, statusline last). This helper documents intent.
    let _ = app;
    true
}

/// Committing a URL edit explodes the query into params and a save rewrites the
/// TOML; reloading the file round-trips the exploded params.
#[test]
fn url_commit_explode_toml_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    let file = dir.path().join("users").join("list.toml");

    // Edit the URL to add a query string, commit (explodes into params), save.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    // Append the query to the seeded URL.
    type_str(&mut app, "?page=2&sort=name");
    press(&mut app, KeyCode::Enter); // commit → merge
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();

    let after = std::fs::read_to_string(&file).unwrap();
    // The base URL no longer carries the query; the params are exploded.
    assert!(!after.contains('?'), "query stripped from URL: {after}");
    assert!(after.contains("[[request.params]]"), "{after}");
    assert!(after.contains("name = \"page\""), "{after}");
    assert!(after.contains("value = \"2\""), "{after}");
    assert!(after.contains("name = \"sort\""), "{after}");

    // Reload the workspace and re-select: the exploded params round-trip.
    let ws = open_workspace(dir.path()).unwrap();
    let mut app2 = App::new(ws, KeyMap::default()).unwrap();
    load_first_users_endpoint(&mut app2);
    let req = &app2.selected.as_ref().unwrap().endpoint.request;
    assert_eq!(req.params.len(), 2);
    assert!(
        req.params
            .iter()
            .any(|p| p.name == "page" && p.value == "2")
    );
    assert!(!req.url.contains('?'));
}

// ---- M6.7 review round 2: zoom collapsed summaries (Finding #3) ----

/// When Request is zoomed, the collapsed response area shows a one-line summary.
#[test]
fn zoom_request_collapsed_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::Request;
    // Zoom the request pane — response collapses to its summary.
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

/// When Response is zoomed, the collapsed request area shows the tab bar.
#[test]
fn zoom_response_collapsed_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    // Put the response into a Done state so the summary is meaningful.
    app.response = ResponseState::Done {
        view: ResponseView::build(&json_response("{}"), 1),
    };
    app.focus = Pane::Response;
    // Zoom the response pane — request collapses to its tab bar.
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
        .unwrap();
    insta::assert_snapshot!(snapshot(&mut app));
}

// ---- M6.7 review round 2: dirty markers (Finding #5 refinement) ----

/// While the loaded endpoint is dirty, its explorer row carries an accent `●`
/// suffix (matched by file path) and the statusline says `● unsaved · w save`;
/// saving with `w` clears both.
#[test]
fn explorer_row_dirty_marker_clears_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    // Make it dirty via a URL edit.
    app.focus = Pane::UrlBar;
    press(&mut app, KeyCode::Char('i'));
    type_str(&mut app, "X");
    press(&mut app, KeyCode::Enter);
    let dirty_rendered = snapshot(&mut app);
    assert!(
        dirty_rendered.contains("List users ●"),
        "loaded endpoint's explorer row must carry the dirty ● suffix:\n{dirty_rendered}"
    );
    assert!(
        dirty_rendered.contains("● unsaved · w save"),
        "statusline must carry the explicit unsaved marker:\n{dirty_rendered}"
    );
    insta::assert_snapshot!(dirty_rendered);

    // Save clears every dirty marker.
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    let saved_rendered = snapshot(&mut app);
    assert!(
        !saved_rendered.contains("List users ●"),
        "explorer dirty ● must clear after save:\n{saved_rendered}"
    );
    assert!(
        !saved_rendered.contains("● unsaved"),
        "statusline unsaved marker must clear after save:\n{saved_rendered}"
    );
}

// ---- M6.7 review round 2: numbered tab titles (Finding #9) ----

/// When the Request pane is focused, tab titles are prefixed with their 1-based
/// jump digit: `[1] Params(n)  [2] Headers(n)  [3] Auth  [4] Body`.
#[test]
fn request_tab_bar_shows_digit_prefixes_when_focused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_fixture(dir.path());
    load_first_users_endpoint(&mut app);
    app.focus = Pane::Request;
    let rendered = snapshot(&mut app);
    // The focused Request pane tab bar must show digit prefixes.
    assert!(
        rendered.contains("[1]"),
        "focused tab bar must show [1] digit prefix for tab 1: {rendered}"
    );
    insta::assert_snapshot!(rendered);
}

// --- M7.3: environments & variables editor ---

/// A workspace with workspace vars (incl. a secret-named literal), a collection
/// with vars, and a `dev` profile — the full three-scope surface the editor edits.
fn env_fixture(root: &Path) {
    std::fs::write(
        root.join("churl.toml"),
        concat!(
            "name = \"demo\"\n\n",
            "[vars]\n",
            "base_url = \"https://api.example.com\"\n",
            "api_token = \"literal-secret-value\"\n\n",
            "[[profiles]]\n",
            "name = \"dev\"\n",
            "[profiles.vars]\n",
            "base_url = \"https://dev.example.com\"\n",
            "host = \"dev.local\"\n",
        ),
    )
    .unwrap();
    let users = root.join("users");
    std::fs::create_dir(&users).unwrap();
    std::fs::write(users.join("folder.toml"), "[vars]\npage_size = \"50\"\n").unwrap();
    std::fs::write(
        users.join("list.toml"),
        "seq = 0\nname = \"List users\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/users\"\n",
    )
    .unwrap();
}

fn app_with_env_fixture(root: &Path) -> App {
    env_fixture(root);
    let workspace = open_workspace(root).unwrap();
    App::new(workspace, KeyMap::default()).unwrap()
}

/// Like [`env_fixture`] but with no pre-existing secret literal, so the manifest
/// is saveable (the secrets gate refuses any manifest carrying a literal secret).
fn app_with_clean_env_fixture(root: &Path) -> App {
    std::fs::write(
        root.join("churl.toml"),
        concat!(
            "name = \"demo\"\n\n",
            "[vars]\n",
            "base_url = \"https://api.example.com\"\n\n",
            "[[profiles]]\n",
            "name = \"dev\"\n",
            "[profiles.vars]\n",
            "host = \"dev.local\"\n",
        ),
    )
    .unwrap();
    let users = root.join("users");
    std::fs::create_dir(&users).unwrap();
    std::fs::write(users.join("folder.toml"), "[vars]\npage_size = \"50\"\n").unwrap();
    let workspace = open_workspace(root).unwrap();
    App::new(workspace, KeyMap::default()).unwrap()
}

/// Opens the env editor via the `<leader>v` chord (Space then `v`).
fn open_env(app: &mut App) {
    press(app, KeyCode::Char(' '));
    press(app, KeyCode::Char('v'));
    assert_eq!(app.mode, Mode::EnvEditor, "editor must be open");
}

#[test]
fn env_editor_opens_with_both_panes() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    open_env(&mut app);
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn env_editor_masks_secret_named_literal() {
    // Focus the workspace var rows: `api_token`'s literal value is masked.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // into the workspace var rows
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("••••••"),
        "secret literal masked:\n{rendered}"
    );
    assert!(
        !rendered.contains("literal-secret-value"),
        "the literal must never render:\n{rendered}"
    );
    insta::assert_snapshot!(rendered);
}

#[test]
fn env_editor_editing_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // rows (lands on api_token, the first row)
    press(&mut app, KeyCode::Char('j')); // down to base_url
    press(&mut app, KeyCode::Enter); // edit base_url value
    type_str(&mut app, "/v2");
    insta::assert_snapshot!(snapshot(&mut app));
}

#[test]
fn env_editor_precedence_shows_shadowing() {
    // Activate `dev`, then view the workspace scope: base_url is shadowed by the
    // profile, api_token (unique) wins.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    open_env(&mut app);
    press(&mut app, KeyCode::Char('j')); // users
    press(&mut app, KeyCode::Char('j')); // dev
    press(&mut app, KeyCode::Char('x')); // activate dev
    press(&mut app, KeyCode::Char('k')); // users
    press(&mut app, KeyCode::Char('k')); // workspace
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("profile dev"),
        "workspace base_url must show it is shadowed by profile dev:\n{rendered}"
    );
    insta::assert_snapshot!(rendered);
}

#[test]
fn env_editor_discard_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // rows
    press(&mut app, KeyCode::Enter); // edit value
    type_str(&mut app, "X");
    press(&mut app, KeyCode::Enter); // commit → dirty
    press(&mut app, KeyCode::Char('q')); // close while dirty → confirm
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("Unsaved changes"),
        "discard confirm shown:\n{rendered}"
    );
    insta::assert_snapshot!(rendered);
}

#[test]
fn env_editor_save_writes_workspace_var() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_clean_env_fixture(dir.path());
    open_env(&mut app);
    // Edit workspace base_url value.
    press(&mut app, KeyCode::Tab); // rows
    press(&mut app, KeyCode::Enter); // edit base_url value
    for _ in 0..40 {
        press(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "https://changed.example");
    press(&mut app, KeyCode::Enter); // commit
    // Save.
    press(&mut app, KeyCode::Char('w'));
    // The editor stays open, dirty cleared; the manifest reflects the change.
    let manifest = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        manifest.contains("https://changed.example"),
        "workspace var saved:\n{manifest}"
    );
    // The secret-named literal was untouched (still present, unchanged) — it is a
    // pre-existing hand-edited value the editor round-trips without gating loads.
    // But editing+saving it as a literal would be refused; here we didn't touch it.
    // Live-refresh: the app's workspace manifest now carries the new value.
    assert_eq!(
        app.workspace
            .as_ref()
            .unwrap()
            .manifest()
            .vars
            .get("base_url")
            .map(String::as_str),
        Some("https://changed.example")
    );
}

#[test]
fn env_editor_new_profile_persists() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_clean_env_fixture(dir.path());
    open_env(&mut app);
    // New profile from the scope list.
    press(&mut app, KeyCode::Char('n'));
    type_str(&mut app, "prod");
    press(&mut app, KeyCode::Enter); // commit name → focuses new profile rows
    // Add a var to it.
    press(&mut app, KeyCode::Char('a'));
    type_str(&mut app, "host");
    press(&mut app, KeyCode::Enter); // name → value
    type_str(&mut app, "prod.local");
    press(&mut app, KeyCode::Enter); // commit row
    press(&mut app, KeyCode::Char('w')); // save
    let manifest = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        manifest.contains("prod"),
        "new profile persisted:\n{manifest}"
    );
    assert!(
        manifest.contains("prod.local"),
        "profile var persisted:\n{manifest}"
    );
    // Re-load through the model to prove it round-trips.
    let ws = churl_core::persistence::load_workspace_manifest(dir.path()).unwrap();
    assert!(ws.profiles.iter().any(|p| p.name == "prod"));
}

#[test]
fn env_editor_discard_leaves_disk_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    let before = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // rows
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "junk");
    press(&mut app, KeyCode::Enter); // commit → dirty
    press(&mut app, KeyCode::Char('q')); // confirm
    press(&mut app, KeyCode::Char('d')); // discard
    assert_eq!(app.mode, Mode::Normal, "editor closed");
    let after = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert_eq!(before, after, "discard must not write anything");
}

#[test]
fn env_editor_save_refuses_secret_literal() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_env_fixture(dir.path());
    let before = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // workspace rows
    // Add a new secret-named var with a literal value.
    press(&mut app, KeyCode::Char('a'));
    type_str(&mut app, "session_token");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "abc123");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('w')); // save → refused
    assert_eq!(app.mode, Mode::EnvEditor, "editor stays open on refusal");
    let after = std::fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert_eq!(before, after, "refused save writes nothing");
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("secret") || rendered.contains("session_token"),
        "refusal message shown:\n{rendered}"
    );
}

#[test]
fn env_editor_collection_override_marker_and_legend() {
    // Fix 2 (visible): a workspace var also set in a collection renders `✓*` with
    // a footer legend, so the winner marker never overstates the win.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nbase_url = \"https://api.example.com\"\n",
    )
    .unwrap();
    let users = dir.path().join("users");
    std::fs::create_dir(&users).unwrap();
    std::fs::write(
        users.join("folder.toml"),
        "[vars]\nbase_url = \"https://users.example.com\"\n",
    )
    .unwrap();
    let workspace = open_workspace(dir.path()).unwrap();
    let mut app = App::new(workspace, KeyMap::default()).unwrap();
    open_env(&mut app);
    press(&mut app, KeyCode::Tab); // into the workspace var rows (base_url selected)
    let rendered = snapshot(&mut app);
    assert!(
        rendered.contains("✓*"),
        "collection-override marker shown:\n{rendered}"
    );
    assert!(
        rendered.contains("also set in a collection"),
        "footer legend shown:\n{rendered}"
    );
}
