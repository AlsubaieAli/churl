//! Integration tests for the `churl init` subcommand, driving the real binary.
//! Replaces the removed `churl tutorial` subcommand (see DECISIONS.md) — the
//! demo scaffold now lives behind `init --demo`.

use std::process::{Command, Output};

fn churl_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn init_blank_scaffolds_only_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["init"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let ws = churl_core::persistence::OpenWorkspace::open(dir.path()).unwrap();
    assert!(
        ws.manifest().vars.is_empty(),
        "blank init should have no vars"
    );
    assert!(
        ws.manifest().profiles.is_empty(),
        "blank init should have no profiles"
    );
    assert!(
        ws.collections().unwrap().is_empty(),
        "blank init should have no collections"
    );
}

#[test]
fn init_path_arg_scaffolds_a_new_directory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("my-api");
    let output = churl_in(dir.path(), &["init", "my-api"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(target.join("churl.toml").exists());
}

#[test]
fn init_refuses_to_overwrite_an_existing_workspace() {
    let dir = tempfile::tempdir().unwrap();
    assert!(churl_in(dir.path(), &["init"]).status.success());

    let output = churl_in(dir.path(), &["init"]);
    assert!(
        !output.status.success(),
        "expected non-zero exit for an existing workspace"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' in stderr: {stderr}"
    );
}

#[test]
fn init_demo_scaffolds_workspace_with_3_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["init", "--demo"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Workspace opens and loads.
    let ws = churl_core::persistence::OpenWorkspace::open(dir.path()).unwrap();
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
        "expected 3 demo endpoints, got {}",
        endpoints.len()
    );
}

#[test]
fn init_demo_prints_next_steps_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["init", "--demo"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("churl"),
        "expected 'churl' in next-steps output: {stdout}"
    );
    assert!(
        stdout.contains("Next steps") || stdout.contains("Initialized"),
        "expected next-steps output: {stdout}"
    );
}

#[test]
fn tutorial_subcommand_is_fully_removed() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["tutorial"]);
    assert!(
        !output.status.success(),
        "`churl tutorial` must no longer exist"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized") || stderr.contains("error"),
        "expected clap's unknown-subcommand error: {stderr}"
    );
}
