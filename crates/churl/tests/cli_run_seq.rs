//! Integration tests for `churl run-seq <name>` — the M8.4.1 headless
//! sequence-run path. Drives the real binary against an in-process `wiremock`
//! server (no real network) and asserts on the NDJSON stream (per-step +
//! summary lines) and exit codes, not just substrings of human text.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use churl_core::assert::{AssertOp, Assertion};
use churl_core::model::{
    Endpoint, Header, Method, OnError, Request, Sequence, SequenceStep, Workspace,
};
use churl_core::persistence::{save_endpoint, save_sequence, save_workspace_manifest};
use serde_json::Value;
use wiremock::matchers::{header, method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn churl_in(dir: &Path, args: &[&str]) -> Output {
    let missing_config = std::env::temp_dir().join(format!(
        "churl-cli-run-seq-test-nonexistent-config-{}.toml",
        std::process::id()
    ));
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .env("CHURL_CONFIG", missing_config)
        .output()
        .expect("failed to spawn churl")
}

/// Parses the `--json` NDJSON stream: one JSON object per non-empty stdout line.
fn ndjson(output: &Output) -> Vec<Value> {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!(
                    "invalid NDJSON line: {err}\nline: {line}\nfull stdout: {stdout}\nstderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
        })
        .collect()
}

/// The single terminal `"type":"summary"` line.
fn summary(lines: &[Value]) -> &Value {
    lines
        .iter()
        .find(|l| l["type"] == "summary")
        .expect("a summary line")
}

/// The `"type":"step"` lines, in stream order.
fn steps(lines: &[Value]) -> Vec<&Value> {
    lines.iter().filter(|l| l["type"] == "step").collect()
}

fn ep(name: &str, request: Request, assertions: Vec<Assertion>) -> Endpoint {
    Endpoint {
        seq: 0,
        name: name.to_owned(),
        assertions,
        request,
    }
}

fn get(url: &str, headers: Vec<Header>) -> Request {
    Request {
        method: Method::Get,
        url: url.to_owned(),
        headers,
        params: vec![],
        body: None,
        auth: None,
        insecure: false,
    }
}

fn status_200() -> Assertion {
    Assertion {
        target: "status".to_owned(),
        op: AssertOp::Eq,
        value: Some("200".to_owned()),
    }
}

/// Builds a workspace with a `login` endpoint (returns a token in its body) and
/// a `me` endpoint that sends `Authorization: Bearer {{token}}`, plus a
/// `checkout` sequence that runs login → me, extracting `token` from login's
/// response. `login_assertions`/`me_assertions` let each test choose the
/// persisted gate; `extract` and `on_error` shape the chain.
#[allow(clippy::too_many_arguments)]
fn scaffold(
    root: &Path,
    base_url: &str,
    login_assertions: Vec<Assertion>,
    me_assertions: Vec<Assertion>,
    extract: BTreeMap<String, String>,
    on_error: OnError,
) {
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

    save_endpoint(
        &root.join("login.toml"),
        &ep(
            "login",
            Request {
                method: Method::Post,
                url: "{{base_url}}/login".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: None,
                insecure: false,
            },
            login_assertions,
        ),
    )
    .unwrap();

    save_endpoint(
        &root.join("me.toml"),
        &ep(
            "me",
            get(
                "{{base_url}}/me",
                vec![Header {
                    name: "Authorization".to_owned(),
                    value: "Bearer {{token}}".to_owned(),
                    enabled: true,
                }],
            ),
            me_assertions,
        ),
    )
    .unwrap();

    std::fs::create_dir_all(root.join("sequences")).unwrap();
    save_sequence(
        &root.join("sequences").join("checkout.toml"),
        &Sequence {
            seq: 0,
            name: "Checkout".to_owned(),
            on_error,
            steps: vec![
                SequenceStep {
                    seq: 1,
                    endpoint: "login.toml".to_owned(),
                    extract,
                    persist: vec![],
                },
                SequenceStep {
                    seq: 2,
                    endpoint: "me.toml".to_owned(),
                    extract: BTreeMap::new(),
                    persist: vec![],
                },
            ],
        },
    )
    .unwrap();
}

fn token_extract() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("token".to_owned(), "$.token".to_owned());
    m
}

/// login → 200 with a token; me → 200 only when it presents the chained token.
async fn happy_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"abc123"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wpath("/me"))
        .and(header("Authorization", "Bearer abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"user":"alice"}"#))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn happy_path_streams_steps_and_summary_and_exits_0() {
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    assert!(
        output.status.success(),
        "exit {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = ndjson(&output);
    let step_lines = steps(&lines);
    assert_eq!(step_lines.len(), 2, "two step lines: {lines:?}");

    // Step 1: login, 200, its own status==200 assertion passes.
    assert_eq!(step_lines[0]["type"], "step");
    assert_eq!(step_lines[0]["command"], "run-seq");
    assert_eq!(step_lines[0]["schema_version"], 1);
    assert_eq!(step_lines[0]["seq"], 1);
    assert_eq!(step_lines[0]["endpoint"], "login.toml");
    assert_eq!(step_lines[0]["ok"], true);
    assert_eq!(step_lines[0]["data"]["response"]["status"], 200);
    assert_eq!(step_lines[0]["data"]["assertions"]["passed"], true);

    // Step 2: me — a 200 here PROVES the chain (the mock only answers 200 when
    // the extracted token rode through as `Authorization: Bearer abc123`).
    assert_eq!(step_lines[1]["seq"], 2);
    assert_eq!(step_lines[1]["endpoint"], "me.toml");
    assert_eq!(step_lines[1]["data"]["response"]["status"], 200);
    assert_eq!(
        step_lines[1]["data"]["response"]["body"],
        r#"{"user":"alice"}"#
    );

    let sum = summary(&lines);
    assert_eq!(sum["ok"], true);
    assert_eq!(sum["sequence"], "checkout");
    assert_eq!(sum["steps"]["total"], 2);
    assert_eq!(sum["steps"]["ran"], 2);
    assert_eq!(sum["steps"]["skipped"], 0);
    assert_eq!(sum["steps"]["failed"], 0);
    assert_eq!(sum["assertions"]["total"], 2);
    assert_eq!(sum["assertions"]["passed"], 2);
    assert_eq!(sum["assertions"]["failed"], 0);
}

#[tokio::test]
async fn per_step_data_is_byte_identical_to_a_single_run() {
    // The step envelope's `data` must be the SAME frozen ExecData a standalone
    // `run` of that endpoint emits (the M8.4.1 "per-step = frozen single-request
    // envelope" contract). Compare login's step `data` to `run login`'s `data`.
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );

    let seq_out = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    let lines = ndjson(&seq_out);
    let login_step = steps(&lines)[0].clone();

    let run_out = churl_in(dir.path(), &["--json", "run", "login"]);
    let run_env: Value =
        serde_json::from_str(String::from_utf8(run_out.stdout).unwrap().trim()).unwrap();

    // Two independent live requests differ only in volatile bits — the server's
    // `Date` response header and the measured `timing_ms`. Normalise those away;
    // everything else (request echo, status, body, encoding, truncated,
    // assertions, and the key set) must be identical, proving the step envelope
    // reuses the exact frozen single-request `data` shape.
    fn normalise(mut data: Value) -> Value {
        let resp = data["response"].as_object_mut().unwrap();
        resp.remove("timing_ms");
        let headers = resp["headers"].as_array_mut().unwrap();
        headers
            .retain(|h| h["name"].as_str().map(str::to_ascii_lowercase).as_deref() != Some("date"));
        data
    }
    assert_eq!(
        normalise(login_step["data"].clone()),
        normalise(run_env["data"].clone()),
        "a sequence step's data must match a standalone run's data (modulo Date/timing)"
    );
}

#[tokio::test]
async fn assertion_failure_exits_1_but_steps_stay_ok_shaped() {
    // me asserts status==500 (it returns 200) → the assertion fails. The run
    // exits 1, but the step's `ok`/`data` stay success-shaped (the request
    // succeeded) — the same exception to "ok mirrors exit" as single-request.
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![Assertion {
            target: "status".to_owned(),
            op: AssertOp::Eq,
            value: Some("500".to_owned()),
        }],
        token_extract(),
        OnError::Halt,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    assert_eq!(output.status.code(), Some(1), "a failed assertion exits 1");

    let lines = ndjson(&output);
    let step_lines = steps(&lines);
    // Both steps ran (no halt — an assertion failure never halts the chain).
    assert_eq!(step_lines.len(), 2);
    assert_eq!(step_lines[1]["ok"], true, "request succeeded");
    assert!(step_lines[1]["error"].is_null());
    assert_eq!(step_lines[1]["data"]["assertions"]["passed"], false);

    let sum = summary(&lines);
    assert_eq!(sum["ok"], false);
    assert_eq!(sum["assertions"]["total"], 2);
    assert_eq!(sum["assertions"]["passed"], 1);
    assert_eq!(sum["assertions"]["failed"], 1);
    assert_eq!(
        sum["steps"]["failed"], 0,
        "an assertion fail is not a step fail"
    );
}

#[tokio::test]
async fn broken_extraction_chain_exits_1_and_surfaces_extract_error() {
    // login returns a token, but the sequence extracts `$.nonexistent` → the
    // chain breaks. Under the default Halt, `me` is skipped. This must NOT exit
    // 0 (the CI footgun): a broken chain fails the run (exit 1), with the reason
    // surfaced on the step line.
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    let mut bad_extract = BTreeMap::new();
    bad_extract.insert("token".to_owned(), "$.nonexistent".to_owned());
    scaffold(
        dir.path(),
        &server.uri(),
        vec![],
        vec![],
        bad_extract,
        OnError::Halt,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a broken extraction chain must fail the run, not silently exit 0"
    );

    let lines = ndjson(&output);
    let step_lines = steps(&lines);
    // login ran (200) but its extraction failed; me was skipped.
    assert_eq!(step_lines[0]["ok"], true, "login request itself succeeded");
    assert!(
        step_lines[0]["extract_error"].is_string(),
        "the broken-chain reason must be surfaced: {:?}",
        step_lines[0]
    );
    assert_eq!(
        step_lines[1]["skipped"], true,
        "the tail is skipped after a halt"
    );

    let sum = summary(&lines);
    assert_eq!(sum["ok"], false);
    assert_eq!(sum["steps"]["ran"], 1);
    assert_eq!(sum["steps"]["skipped"], 1);
    assert_eq!(sum["steps"]["failed"], 1);
}

#[tokio::test]
async fn halt_on_asserted_http_error_skips_the_tail_and_exits_1() {
    // login returns 500; its persisted `status==200` assertion fails AND the
    // ≥400 status halts the run (default Halt) → me is skipped. Exit 1.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/login"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    assert_eq!(output.status.code(), Some(1));

    let lines = ndjson(&output);
    let step_lines = steps(&lines);
    assert_eq!(step_lines[0]["data"]["response"]["status"], 500);
    assert_eq!(step_lines[0]["data"]["assertions"]["passed"], false);
    assert_eq!(step_lines[1]["skipped"], true);

    let sum = summary(&lines);
    assert_eq!(sum["ok"], false);
    assert_eq!(sum["steps"]["ran"], 1);
    assert_eq!(sum["steps"]["skipped"], 1);
    // The ≥400 step counts as a failed step in the tally.
    assert_eq!(sum["steps"]["failed"], 1);
}

#[tokio::test]
async fn on_error_continue_runs_the_tail_after_a_failure() {
    // login returns 500 (no assertion); with on_error=Continue the run does NOT
    // halt, so me still runs. me has no token (login didn't extract on a 500),
    // so `{{token}}` is unresolved → me is a hard resolution error (band 3).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/login"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    // me answers 200 for any Authorization — but it should never get that far
    // cleanly, because {{token}} can't resolve.
    Mock::given(method("GET"))
        .and(wpath("/me"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![],
        vec![],
        token_extract(),
        OnError::Continue,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    let lines = ndjson(&output);
    let step_lines = steps(&lines);
    assert_eq!(step_lines.len(), 2, "continue runs both steps: {lines:?}");
    assert!(
        !step_lines[1]["skipped"].as_bool().unwrap_or(false),
        "me is not skipped under Continue"
    );
    // me's `{{token}}` never resolved → a band-3 hard error rides the stream.
    assert_eq!(step_lines[1]["ok"], false);
    assert_eq!(step_lines[1]["error"]["kind"], "unresolved-var");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a hard error wins the exit band"
    );
    // The summary's `ok` must NOT lie on a hard-error run — it is derived from
    // the exit code, so a green summary on an exit-3 run is impossible.
    assert_eq!(
        summary(&lines)["ok"],
        false,
        "summary.ok must be false on a hard-error run"
    );
}

#[test]
fn sequence_not_found_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        "http://example.invalid",
        vec![],
        vec![],
        token_extract(),
        OnError::Halt,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "ghost"]);
    assert_eq!(output.status.code(), Some(3));
    let env: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "sequence-not-found");
    assert_eq!(env["command"], "run-seq");
}

#[test]
fn no_workspace_is_exit_3_no_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["--json", "run-seq", "anything"]);
    assert_eq!(output.status.code(), Some(3));
    let env: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(env["error"]["kind"], "no-workspace");
}

#[tokio::test]
async fn human_mode_writes_checklist_to_stderr_and_keeps_stdout_empty() {
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );

    // No --json: human checklist on stderr, machine stdout stays empty.
    let output = churl_in(dir.path(), &["run-seq", "checkout"]);
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "human mode must not print NDJSON to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("login.toml"),
        "checklist names steps: {stderr}"
    );
    assert!(stderr.contains("PASS"), "summary verdict present: {stderr}");
}

// ---- gate follow-ups (M8.4.1 adversarial review) ---------------------------

/// A workspace with one endpoint `solo` at `url` (+ `assertions`, `profiles`)
/// and a 1-step sequence `solo.toml` that runs it — for the flag-plumbing and
/// single-step-boundary cases.
fn scaffold_solo(
    root: &Path,
    base_url: &str,
    url: &str,
    assertions: Vec<Assertion>,
    profiles: Vec<churl_core::model::Profile>,
) {
    let mut vars = BTreeMap::new();
    vars.insert("base_url".to_owned(), base_url.to_owned());
    save_workspace_manifest(
        root,
        &Workspace {
            name: "fixture".to_owned(),
            vars,
            profiles,
            ..Default::default()
        },
    )
    .unwrap();
    save_endpoint(
        &root.join("solo.toml"),
        &ep("solo", get(url, vec![]), assertions),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("sequences")).unwrap();
    save_sequence(
        &root.join("sequences").join("solo.toml"),
        &Sequence {
            seq: 0,
            name: "Solo".to_owned(),
            on_error: OnError::Halt,
            steps: vec![SequenceStep {
                seq: 1,
                endpoint: "solo.toml".to_owned(),
                extract: BTreeMap::new(),
                persist: vec![],
            }],
        },
    )
    .unwrap();
}

#[tokio::test]
async fn cli_var_reaches_a_step() {
    // The step's URL needs `{{tag}}`, supplied only via `--var`. The mock answers
    // 200 only on `?tag=xyz`; if `--var` were dropped, the placeholder would be
    // unresolved (a hard error) — so a clean 200 proves the flag reached the step.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/thing"))
        .and(wiremock::matchers::query_param("tag", "xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_solo(
        dir.path(),
        &server.uri(),
        "{{base_url}}/thing?tag={{tag}}",
        vec![status_200()],
        vec![],
    );

    let output = churl_in(
        dir.path(),
        &["--json", "--var", "tag=xyz", "run-seq", "solo"],
    );
    assert!(
        output.status.success(),
        "exit {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = ndjson(&output);
    let step = steps(&lines)[0];
    assert_eq!(step["data"]["response"]["status"], 200);
    assert!(
        step["data"]["request"]["url"]
            .as_str()
            .unwrap()
            .contains("tag=xyz"),
        "the --var value must appear in the resolved request url: {step:?}"
    );
}

#[tokio::test]
async fn profile_changes_step_resolution() {
    // The step echoes `{{who}}`; a `dev` profile sets who=devil. --profile dev
    // must win the resolution, observably changing the echoed request url.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/hi"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut dev_vars = BTreeMap::new();
    dev_vars.insert("who".to_owned(), "devil".to_owned());
    scaffold_solo(
        dir.path(),
        &server.uri(),
        "{{base_url}}/hi?who={{who}}",
        vec![status_200()],
        vec![churl_core::model::Profile {
            name: "dev".to_owned(),
            vars: dev_vars,
        }],
    );

    let output = churl_in(
        dir.path(),
        &["--json", "--profile", "dev", "run-seq", "solo"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = ndjson(&output);
    assert!(
        steps(&lines)[0]["data"]["request"]["url"]
            .as_str()
            .unwrap()
            .contains("who=devil"),
        "the profile var must win"
    );
}

#[test]
fn unknown_profile_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_solo(
        dir.path(),
        "http://example.invalid",
        "{{base_url}}/x",
        vec![],
        vec![],
    );
    let output = churl_in(
        dir.path(),
        &["--json", "--profile", "ghost", "run-seq", "solo"],
    );
    assert_eq!(output.status.code(), Some(3));
    let env: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(env["error"]["kind"], "unknown-profile");
}

#[tokio::test]
async fn hard_error_wins_exit_band_over_a_failed_assertion() {
    // A run with BOTH a failed assertion (step 1) AND a hard error (step 2, an
    // unresolvable `{{token}}`, since login didn't extract) under Continue must
    // exit with the hard band (3), not the assertion band (1).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        // login: a failing assertion (body has no token) + a 200 so it does not halt.
        vec![Assertion {
            target: "$.token".to_owned(),
            op: AssertOp::Exists,
            value: None,
        }],
        vec![],
        token_extract(), // extracts $.token — absent, but 200 so extraction fails...
        OnError::Continue,
    );

    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    // login: 200, assertion `$.token exists` fails AND extraction `$.token` fails.
    // me (continue): `{{token}}` unresolved → band-3 hard error. Band wins over 1.
    assert_eq!(
        output.status.code(),
        Some(3),
        "a hard error band must beat the assertion/extraction exit-1"
    );
    assert_eq!(summary(&ndjson(&output))["ok"], false);
}

#[test]
fn step_referencing_a_missing_endpoint_is_endpoint_not_found() {
    // A sequence step pointing at a non-existent endpoint file → prepare fails →
    // a band-3 `endpoint-not-found` hard error rides the stream, exit 3.
    let dir = tempfile::tempdir().unwrap();
    save_workspace_manifest(
        dir.path(),
        &Workspace {
            name: "fixture".to_owned(),
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("sequences")).unwrap();
    save_sequence(
        &dir.path().join("sequences").join("broken.toml"),
        &Sequence {
            seq: 0,
            name: "Broken".to_owned(),
            on_error: OnError::Halt,
            steps: vec![SequenceStep {
                seq: 1,
                endpoint: "ghost.toml".to_owned(),
                extract: BTreeMap::new(),
                persist: vec![],
            }],
        },
    )
    .unwrap();

    let output = churl_in(dir.path(), &["--json", "run-seq", "broken"]);
    assert_eq!(output.status.code(), Some(3));
    let lines = ndjson(&output);
    assert_eq!(steps(&lines)[0]["error"]["kind"], "endpoint-not-found");
    assert_eq!(steps(&lines)[0]["ok"], false);
}

#[test]
fn empty_sequence_runs_clean() {
    // A sequence with zero steps: no step lines, one summary, exit 0.
    let dir = tempfile::tempdir().unwrap();
    save_workspace_manifest(
        dir.path(),
        &Workspace {
            name: "fixture".to_owned(),
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("sequences")).unwrap();
    save_sequence(
        &dir.path().join("sequences").join("empty.toml"),
        &Sequence {
            seq: 0,
            name: "Empty".to_owned(),
            on_error: OnError::Halt,
            steps: vec![],
        },
    )
    .unwrap();

    let output = churl_in(dir.path(), &["--json", "run-seq", "empty"]);
    assert!(output.status.success());
    let lines = ndjson(&output);
    assert_eq!(steps(&lines).len(), 0, "no steps");
    let sum = summary(&lines);
    assert_eq!(sum["ok"], true);
    assert_eq!(sum["steps"]["total"], 0);
    assert_eq!(sum["assertions"]["total"], 0);
}

#[tokio::test]
async fn summary_is_the_single_terminal_line() {
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );
    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout"]);
    let lines = ndjson(&output);
    assert_eq!(
        lines.iter().filter(|l| l["type"] == "summary").count(),
        1,
        "exactly one summary line"
    );
    assert_eq!(
        lines.last().unwrap()["type"],
        "summary",
        "the summary must be the terminal line"
    );
}

#[tokio::test]
async fn verbose_json_step_trace_is_masked_and_carries_chained_var_steps() {
    // -v --json: each step gets a masked `data.trace`. The `me` step's trace must
    // record the CHAINED `token` var (from the ephemeral `extracted` scope) — the
    // whole point of tracing a sequence — and mask it (a secret-named var).
    let server = happy_server().await;
    let dir = tempfile::tempdir().unwrap();
    scaffold(
        dir.path(),
        &server.uri(),
        vec![status_200()],
        vec![status_200()],
        token_extract(),
        OnError::Halt,
    );
    let output = churl_in(dir.path(), &["--json", "run-seq", "checkout", "-v"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = ndjson(&output);
    let step_lines = steps(&lines);

    // login step: trace present, cookie_used false (no jar), and its request-echo
    // masking holds (the Authorization header, if any, would be ••••••).
    let login_trace = &step_lines[0]["data"]["trace"];
    assert!(
        !login_trace.is_null(),
        "step trace present under -v: {:?}",
        step_lines[0]
    );
    assert_eq!(login_trace["decisions"]["cookie_used"], false);

    // me step: its var_steps must include the chained `token`, masked (••••••),
    // and the real token must not leak anywhere in the step line.
    let me_trace = &step_lines[1]["data"]["trace"];
    let var_steps = me_trace["var_steps"].as_array().expect("var_steps array");
    let token_step = var_steps
        .iter()
        .find(|s| s["name"] == "token")
        .expect("the chained `token` var must be recorded in the me step's trace");
    assert_eq!(
        token_step["value_masked"], "••••••",
        "a chained secret-named var must be masked in the trace"
    );
    // The real token value never appears in the me step's request echo/trace.
    let me_str = serde_json::to_string(&step_lines[1]).unwrap();
    // (`abc123` may appear in login's response BODY legitimately, but the me step
    // never echoes the token except masked.)
    assert!(
        !me_str.contains("Bearer abc123"),
        "the resolved bearer token must not appear unmasked: {me_str}"
    );
}
