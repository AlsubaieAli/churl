//! Snapshot tests for the M2 layout: three panes, overlays, and the empty
//! state, driven through real key events against a `TestBackend` (no tokio).

use std::path::Path;
use std::time::{Duration, Instant};

use churl::tui::app::{App, Pane, open_workspace, render};
use churl::tui::components::response::{ResponseMeta, ResponseState, ResponseView};
use churl::tui::events::KeyMap;
use churl_core::model::{Header, Response, Timing};
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
