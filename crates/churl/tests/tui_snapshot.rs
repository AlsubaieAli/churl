//! Snapshot tests for the M2 layout: three panes, overlays, and the empty
//! state, driven through real key events against a `TestBackend` (no tokio).

use std::path::Path;

use churl::tui::app::{App, open_workspace, render};
use churl::tui::events::KeyMap;
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
