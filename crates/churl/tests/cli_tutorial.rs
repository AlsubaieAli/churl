//! Integration tests for the `churl tutorial` subcommand, driving the real binary.

use std::process::{Command, Output};

fn churl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn tutorial_scaffolds_workspace_with_3_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().join("tutorial-ws");
    let output = churl(&["tutorial", "--dir", workspace_dir.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Workspace opens and loads.
    let ws = churl_core::persistence::OpenWorkspace::open(&workspace_dir).unwrap();
    assert_eq!(ws.manifest().name, "churl-tutorial");
    assert_eq!(
        ws.manifest().vars.get("base_url").map(String::as_str),
        Some("https://httpbingo.org")
    );
    assert!(
        !ws.manifest().profiles.is_empty(),
        "should have at least one profile"
    );
    // The dev profile overrides `greeting` so profile switching is visible.
    assert_eq!(
        ws.manifest().vars.get("greeting").map(String::as_str),
        Some("hello")
    );
    let dev = &ws.manifest().profiles[0];
    assert_eq!(
        dev.vars.get("greeting").map(String::as_str),
        Some("hello-from-dev")
    );

    // The examples collection exists and has 3 endpoints.
    let collections = ws.collections().unwrap();
    assert_eq!(collections.len(), 1, "expected 1 collection");
    let endpoints: Vec<_> = collections[0].endpoints().unwrap();
    assert_eq!(
        endpoints.len(),
        3,
        "expected 3 tutorial endpoints, got {}",
        endpoints.len()
    );
}

#[test]
fn tutorial_refuses_overwrite_of_non_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().join("tutorial-ws");

    // Create a non-empty directory manually.
    std::fs::create_dir_all(&workspace_dir).unwrap();
    std::fs::write(workspace_dir.join("something.txt"), "not empty").unwrap();

    let output = churl(&["tutorial", "--dir", workspace_dir.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "expected non-zero exit for non-empty dir"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not empty") || stderr.contains("already exists"),
        "expected 'not empty' or 'already exists' in stderr: {stderr}"
    );
}

#[test]
fn tutorial_prints_next_steps_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().join("tutorial-ws2");
    let output = churl(&["tutorial", "--dir", workspace_dir.to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("churl"),
        "expected 'churl' in next-steps output: {stdout}"
    );
    assert!(
        stdout.contains("Next steps") || stdout.contains("cd"),
        "expected next-steps output: {stdout}"
    );
}
