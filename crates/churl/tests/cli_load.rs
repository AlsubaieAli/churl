//! Integration tests for `churl load <endpoint>` — the M8.4.2 headless load-run
//! path. Drives the real binary against an in-process `wiremock` server (no real
//! network) and asserts on the frozen aggregate envelope bytes + exit codes.
//! Mirrors `cli_run.rs`/`cli_run_seq.rs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use churl_core::model::{Endpoint, Method, Request, Workspace};
use churl_core::persistence::{create_endpoint, save_endpoint, save_workspace_manifest};
use serde_json::Value;
use wiremock::matchers::{method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn churl_in(dir: &Path, args: &[&str]) -> Output {
    let missing_config = std::env::temp_dir().join(format!(
        "churl-cli-load-test-nonexistent-config-{}.toml",
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

/// A workspace with one root-level "Hit" endpoint pointing at `{{base_url}}/hit`.
fn scaffold_workspace(root: &Path, base_url: &str) {
    let mut vars = BTreeMap::new();
    vars.insert("base_url".to_owned(), base_url.to_owned());
    save_workspace_manifest(
        root,
        &Workspace {
            name: "fixture".to_owned(),
            vars,
            ..Default::default()
        },
    )
    .unwrap();

    let ep_path = create_endpoint(root, "Hit").unwrap();
    save_endpoint(
        &ep_path,
        &Endpoint {
            seq: 0,
            name: "Hit".to_owned(),
            assertions: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/hit".to_owned(),
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

/// Mounts an all-200 mock on `/hit`.
async fn mount_ok(server: &MockServer) {
    Mock::given(method("GET"))
        .and(wpath("/hit"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(server)
        .await;
}

/// Mounts a mix: the first `failures` requests get 500, the rest get 200. The
/// 500 mock has higher priority (1) and is capped via `up_to_n_times`, so once
/// exhausted the 200 mock serves — a deterministic count regardless of the
/// concurrent arrival order.
async fn mount_mixed(server: &MockServer, failures: u64) {
    Mock::given(method("GET"))
        .and(wpath("/hit"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(failures)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(wpath("/hit"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn load_happy_path_reports_aggregate_stats() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "load", "Hit", "--total", "5"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let env = envelope(&output);
    assert_eq!(env["schema_version"], 1);
    assert_eq!(env["ok"], true);
    assert_eq!(env["command"], "load");
    let stats = &env["data"]["stats"];
    assert_eq!(stats["count"], 5);
    assert_eq!(stats["ok"], 5);
    assert_eq!(stats["failed"], 0);
    assert_eq!(stats["errored"], 0);
    assert_eq!(stats["success_rate"], 1.0);
    assert_eq!(stats["error_rate"], 0.0);
    // Latencies present (something completed) and rps computed.
    assert!(!stats["p95_ms"].is_null(), "{env}");
    assert!(!stats["rps"].is_null(), "{env}");
    // No --assert → assertions stays null (back-compat with run/send).
    assert!(env["data"]["assertions"].is_null());
}

#[tokio::test]
async fn load_flags_reach_the_run() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &[
            "--json",
            "load",
            "Hit",
            "--total",
            "8",
            "--concurrency",
            "4",
            "--gap",
            "5",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    // The config is echoed…
    assert_eq!(env["data"]["config"]["total"], 8);
    assert_eq!(env["data"]["config"]["concurrency"], 4);
    assert_eq!(env["data"]["config"]["gap_ms"], 5);
    // …and `--total` actually drove the run (8 requests attempted).
    assert_eq!(env["data"]["stats"]["count"], 8);
}

#[tokio::test]
async fn load_mixed_status_counts_and_rate_math() {
    let server = MockServer::start().await;
    // total 4, 1 failure → error_rate 0.25, success_rate 0.75 (exact in f64).
    mount_mixed(&server, 1).await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "load", "Hit", "--total", "4"]);
    assert!(output.status.success());
    let stats = &envelope(&output)["data"]["stats"];
    assert_eq!(stats["count"], 4);
    assert_eq!(stats["ok"], 3);
    assert_eq!(stats["failed"], 1);
    assert_eq!(stats["errored"], 0);
    assert_eq!(stats["success_rate"], 0.75);
    assert_eq!(stats["error_rate"], 0.25);
}

#[tokio::test]
async fn load_passing_stat_assertion_exits_0() {
    let server = MockServer::start().await;
    mount_mixed(&server, 1).await; // error_rate 0.25
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &[
            "--json",
            "load",
            "Hit",
            "--total",
            "4",
            "--assert",
            "stats.error_rate <= 0.25",
            "--assert",
            "stats.count == 4",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["assertions"]["passed"], true);
    assert_eq!(env["data"]["assertions"]["total"], 2);
    assert_eq!(env["data"]["assertions"]["failed"], 0);
}

#[tokio::test]
async fn load_failing_stat_assertion_exits_1_but_stays_success_shaped() {
    let server = MockServer::start().await;
    mount_mixed(&server, 1).await; // error_rate 0.25
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &[
            "--json",
            "load",
            "Hit",
            "--total",
            "4",
            "--assert",
            "stats.error_rate < 0.1",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed stats assertion must exit 1"
    );
    let env = envelope(&output);
    // The run itself completed — envelope stays success-shaped, the documented
    // exception to "ok mirrors the exit code".
    assert_eq!(env["ok"], true, "{env}");
    assert!(!env["data"].is_null());
    assert!(env["error"].is_null());
    assert_eq!(env["data"]["assertions"]["passed"], false);
    assert_eq!(env["data"]["assertions"]["failed"], 1);
    assert_eq!(env["data"]["assertions"]["results"][0]["actual"], "0.25");
}

#[test]
fn load_unknown_stat_target_is_exit_5_invalid_assertion() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(
        dir.path(),
        &["--json", "load", "Hit", "--assert", "stats.bogus < 1"],
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "an unknown stats target is a usage/input error caught pre-flight"
    );
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "invalid-assertion");
}

#[test]
fn load_response_target_is_rejected_as_invalid_assertion() {
    // A response assertion target (no `stats.` prefix) is meaningless for a load
    // run — it must be a pre-flight invalid-assertion, not silently ignored.
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(
        dir.path(),
        &["--json", "load", "Hit", "--assert", "status == 200"],
    );
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(envelope(&output)["error"]["kind"], "invalid-assertion");
}

#[test]
fn load_without_a_workspace_is_exit_3_no_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["--json", "load", "anything"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert_eq!(env["error"]["kind"], "no-workspace");
}

#[test]
fn load_endpoint_not_found_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");
    let output = churl_in(dir.path(), &["--json", "load", "Nonexistent"]);
    assert_eq!(output.status.code(), Some(3));
    let env = envelope(&output);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "endpoint-not-found");
}

#[tokio::test]
async fn load_human_mode_prints_stats_to_stderr_stdout_empty() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    // No --json → human mode. Add a passing assertion so the checklist prints.
    let output = churl_in(
        dir.path(),
        &[
            "load",
            "Hit",
            "--total",
            "3",
            "--assert",
            "stats.count == 3",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A load run has no single body — stdout stays empty, all on stderr.
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty in human mode: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("attempted"), "stats summary: {stderr}");
    assert!(stderr.contains("3 attempted"), "{stderr}");
    assert!(stderr.contains("passed"), "assertion checklist: {stderr}");
}
