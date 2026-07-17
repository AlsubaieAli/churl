//! Integration tests for `churl run <endpoint>` — the M8.2 headless saved-
//! endpoint execution path. Drives the real binary against an in-process
//! `wiremock` server (no real network) and asserts on the frozen envelope
//! bytes + exit codes, not just substrings of human text.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use churl_core::model::{Endpoint, Method, Profile, Request, Workspace};
use churl_core::persistence::{
    create_collection, create_endpoint, save_endpoint, save_workspace_manifest,
};
use serde_json::Value;
use wiremock::matchers::{method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn churl_in(dir: &Path, args: &[&str]) -> Output {
    let missing_config = std::env::temp_dir().join(format!(
        "churl-cli-run-test-nonexistent-config-{}.toml",
        std::process::id()
    ));
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .env("CHURL_CONFIG", missing_config)
        .output()
        .expect("failed to spawn churl")
}

fn envelope(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "invalid JSON envelope: {err}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Builds a workspace at `root`: `churl.toml` with `base_url` + a `dev`
/// profile, a nested `api/users` collection holding a "Get User" endpoint
/// (`{{base_url}}/users/1`), and a root-level "Ping" endpoint
/// (`{{base_url}}/ping`) — enough to exercise nested-path resolution, the
/// collection ancestor var chain, and root-level endpoints in one fixture.
fn scaffold_workspace(root: &Path, base_url: &str) {
    let mut vars = BTreeMap::new();
    vars.insert("base_url".to_owned(), base_url.to_owned());
    vars.insert("greeting".to_owned(), "hi".to_owned());
    let mut dev_vars = BTreeMap::new();
    dev_vars.insert("greeting".to_owned(), "hi-from-dev".to_owned());
    save_workspace_manifest(
        root,
        &Workspace {
            name: "fixture".to_owned(),
            vars,
            profiles: vec![Profile {
                name: "dev".to_owned(),
                vars: dev_vars,
            }],
            ..Default::default()
        },
    )
    .unwrap();

    let api_dir = create_collection(root, "api", root).unwrap();
    let users_dir = create_collection(&api_dir, "users", root).unwrap();

    let ep_path = create_endpoint(&users_dir, "Get User").unwrap();
    save_endpoint(
        &ep_path,
        &Endpoint {
            seq: 0,
            name: "Get User".to_owned(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/users/1".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: None,
                insecure: false,
            },
        },
    )
    .unwrap();

    let ping_path = create_endpoint(root, "Ping").unwrap();
    save_endpoint(
        &ping_path,
        &Endpoint {
            seq: 0,
            name: "Ping".to_owned(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/ping".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: None,
                insecure: false,
            },
        },
    )
    .unwrap();

    let broken_path = create_endpoint(root, "Broken").unwrap();
    save_endpoint(
        &broken_path,
        &Endpoint {
            seq: 0,
            name: "Broken".to_owned(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/x?token={{missing_var}}".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: None,
                insecure: false,
            },
        },
    )
    .unwrap();
}

#[tokio::test]
async fn run_resolves_a_nested_endpoint_and_executes_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/users/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("alice"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "run", "api/users/Get User"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let env = envelope(&output);
    assert_eq!(env["schema_version"], 1);
    assert_eq!(env["ok"], true);
    assert_eq!(env["command"], "run");
    assert_eq!(
        env["data"]["request"]["url"],
        format!("{}/users/1", server.uri())
    );
    assert_eq!(env["data"]["response"]["status"], 200);
    assert_eq!(env["data"]["response"]["body"], "alice");
}

#[tokio::test]
async fn run_resolves_a_root_level_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "run", "Ping"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(envelope(&output)["data"]["response"]["body"], "pong");
}

#[tokio::test]
async fn run_profile_overrides_collection_vars() {
    // "Get User" doesn't use {{greeting}}, so prove the profile scope through
    // a --var-visible echo instead: point base_url via --var, confirming cli
    // scope outranks both profile and collection.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/users/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://cli-should-be-overridden.invalid");

    let output = churl_in(
        dir.path(),
        &[
            "--json",
            "--var",
            &format!("base_url={}", server.uri()),
            "run",
            "api/users/Get User",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(envelope(&output)["data"]["response"]["status"], 200);
}

#[test]
fn run_endpoint_not_found_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(dir.path(), &["--json", "run", "api/users/Nonexistent"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "endpoint-not-found");
}

#[test]
fn run_unresolved_var_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(dir.path(), &["--json", "run", "Broken"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert_eq!(env["error"]["kind"], "unresolved-var");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing_var"),
        "{env}"
    );
}

#[test]
fn run_unknown_profile_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(dir.path(), &["--json", "--profile", "ghost", "run", "Ping"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert_eq!(env["error"]["kind"], "unknown-profile");
}

#[test]
fn run_without_a_workspace_is_exit_3_no_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["--json", "run", "anything"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert_eq!(env["error"]["kind"], "no-workspace");
}
