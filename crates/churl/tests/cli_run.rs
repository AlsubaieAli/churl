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
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
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
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
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
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
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

    // Uses {{greeting}} in the URL query so the profile scope (dev overrides
    // greeting) is observable in the echoed request.url.
    let greet_path = create_endpoint(root, "Greet").unwrap();
    save_endpoint(
        &greet_path,
        &Endpoint {
            seq: 0,
            name: "Greet".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/greet?g={{greeting}}".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: None,
                insecure: false,
            },
        },
    )
    .unwrap();

    // A secret-named query value the resolver substitutes in — must be masked
    // in the echoed request.url.
    let secret_path = create_endpoint(root, "Secret Query").unwrap();
    save_endpoint(
        &secret_path,
        &Endpoint {
            seq: 0,
            name: "Secret Query".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/data?api_key={{apikey}}".to_owned(),
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
    // The `Greet` endpoint's URL is `{{base_url}}/greet?g={{greeting}}`. The root
    // collection sets greeting=hi; the `dev` profile overrides it to
    // hi-from-dev. Passing --profile dev must win over the collection scope,
    // observably changing the echoed request.url.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/greet"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    // With --profile dev: the profile's greeting wins.
    let with_profile = churl_in(dir.path(), &["--json", "--profile", "dev", "run", "Greet"]);
    assert!(
        with_profile.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with_profile.stderr)
    );
    let url = envelope(&with_profile)["data"]["request"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(url.contains("g=hi-from-dev"), "profile should win: {url}");

    // Without --profile: the collection's greeting=hi is used.
    let without = churl_in(dir.path(), &["--json", "run", "Greet"]);
    assert!(without.status.success());
    let url2 = envelope(&without)["data"]["request"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(url2.contains("g=hi"), "collection scope: {url2}");
    assert!(!url2.contains("hi-from-dev"), "no profile applied: {url2}");
}

#[tokio::test]
async fn run_masks_secret_query_value_in_echoed_request_url() {
    // P0-2 on the `run` surface: a {{apikey}}-substituted secret-named query
    // value must be masked in data.request.url (the real request still sent it).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/data"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &["--json", "--var", "apikey=REALKEY", "run", "Secret Query"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let echoed = envelope(&output)["data"]["request"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !echoed.contains("REALKEY"),
        "secret leaked in url: {echoed}"
    );
    assert!(echoed.contains("api_key="), "key name kept: {echoed}");
}

#[test]
fn run_corrupt_global_config_is_exit_3_config_error_not_exit_1() {
    // P0-1 on the `run` surface: a malformed global config surfaces a band-3
    // envelope, never an exit-1 bubble with empty stdout.
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");
    let cfg = dir.path().join("bad-config.toml");
    std::fs::write(&cfg, "redirect = \"nonsense\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(["--json", "run", "Ping"])
        .current_dir(dir.path())
        .env("CHURL_CONFIG", &cfg)
        .output()
        .expect("spawn churl");
    assert_eq!(
        output.status.code(),
        Some(3),
        "must be band 3, never exit 1"
    );
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert_eq!(env["error"]["kind"], "config-error");
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

// ---- M8.4: assertions ------------------------------------------------------

#[tokio::test]
async fn run_assertion_free_call_keeps_assertions_null() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "run", "Ping"]);
    assert!(output.status.success());
    assert!(
        envelope(&output)["data"]["assertions"].is_null(),
        "M8.2 back-compat: no assertions given must keep the key null"
    );
}

#[tokio::test]
async fn run_assert_flag_passes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &["--json", "run", "Ping", "--assert", "status == 200"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["assertions"]["passed"], true);
    assert_eq!(env["data"]["assertions"]["total"], 1);
    assert_eq!(env["data"]["assertions"]["failed"], 0);
}

#[tokio::test]
async fn run_assert_flag_failure_exits_1_but_stays_success_shaped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &["--json", "run", "Ping", "--assert", "status == 500"],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed assertion set must exit 1"
    );
    let env = envelope(&output);
    // The request itself succeeded — `ok`/`data` stay success-shaped, the
    // sole documented exception to "ok mirrors the exit code" (docs/CLI.md).
    assert_eq!(env["ok"], true, "{env}");
    assert!(!env["data"].is_null());
    assert!(env["error"].is_null());
    assert_eq!(env["data"]["assertions"]["passed"], false);
    assert_eq!(env["data"]["assertions"]["failed"], 1);
}

#[test]
fn run_invalid_assert_flag_is_exit_5_invalid_assertion() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), "http://example.invalid");

    let output = churl_in(
        dir.path(),
        &["--json", "run", "Ping", "--assert", "status ?? 200"],
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "an unparseable --assert is a usage/input error, never a request failure"
    );
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "invalid-assertion");
}

#[tokio::test]
async fn run_persisted_assertions_run_before_cli_assert_flags() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    // Persist one passing assertion directly onto the root-level Ping endpoint.
    let ping_path = dir.path().join("ping.toml");
    let mut ep = churl_core::persistence::load_endpoint(&ping_path).unwrap();
    ep.assertions = vec![churl_core::assert::Assertion {
        target: "status".to_owned(),
        op: churl_core::assert::AssertOp::Eq,
        value: Some("200".to_owned()),
    }];
    churl_core::persistence::save_endpoint(&ping_path, &ep).unwrap();

    // The CLI flag appends a second, failing assertion.
    let output = churl_in(
        dir.path(),
        &["--json", "run", "Ping", "--assert", "$.missing exists"],
    );
    assert_eq!(output.status.code(), Some(1));
    let env = envelope(&output);
    assert_eq!(env["data"]["assertions"]["total"], 2);
    assert_eq!(env["data"]["assertions"]["results"][0]["pass"], true);
    assert_eq!(env["data"]["assertions"]["results"][1]["pass"], false);
}

// ---- M8.3: debug trace (-v --json) -----------------------------------------

#[tokio::test]
async fn run_non_verbose_json_has_no_trace_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(dir.path(), &["--json", "run", "Ping"]);
    assert!(output.status.success());
    let env = envelope(&output);
    let data = env["data"].as_object().expect("data is an object");
    assert!(
        !data.contains_key("trace"),
        "non-verbose data must omit `trace` entirely: {env}"
    );
}

#[tokio::test]
async fn run_verbose_json_includes_trace_with_masked_secret_query() {
    // Reuses the `Secret Query` fixture endpoint ({{apikey}}-substituted
    // secret-named query value) to prove `run`'s -v --json trace masks the
    // same way `data.request` already does, on the `run` (persisted-
    // endpoint) surface rather than `send`'s ad-hoc one.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/data"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    scaffold_workspace(dir.path(), &server.uri());

    let output = churl_in(
        dir.path(),
        &[
            "--json",
            "--var",
            "apikey=REALKEY",
            "run",
            "Secret Query",
            "-v",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["schema_version"], 1);
    let trace = &env["data"]["trace"];
    assert!(
        !trace.is_null(),
        "trace must be present under -v --json: {env}"
    );

    let var_steps = trace["var_steps"].as_array().expect("var_steps array");
    let step = var_steps
        .iter()
        .find(|s| s["name"] == "apikey")
        .expect("apikey var step recorded");
    assert_eq!(step["value_masked"], "••••••", "secret-named var must mask");

    let echoed_url = trace["resolved_display"]["url"].as_str().unwrap();
    assert!(
        !echoed_url.contains("REALKEY"),
        "secret leaked in trace resolved_display.url: {echoed_url}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("REALKEY"),
        "secret leaked anywhere in the envelope: {stdout}"
    );
}

/// U3 headless contract: `churl run` NEVER performs endpoint-level extraction. A
/// single headless process cannot carry a Session var to a later one, so an
/// endpoint's `extract`/`persist` rules are inert on the headless path. An
/// endpoint that carries capture rules produces a `data` envelope identical to a
/// rule-free twin (same `schema_version`, same response, no new keys). Pins the
/// frozen M8.2 output contract against U3.
#[tokio::test]
async fn run_ignores_endpoint_extract_rules_headless() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"token":"S3cr3t"}}"#))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    save_workspace_manifest(
        root,
        &Workspace {
            name: "fx".to_owned(),
            ..Default::default()
        },
    )
    .unwrap();

    let url = format!("{}/login", server.uri());
    let req = || Request {
        method: Method::Get,
        url: url.clone(),
        headers: vec![],
        params: vec![],
        body: None,
        auth: None,
        insecure: false,
    };

    // Endpoint carrying capture rules.
    let mut extract = BTreeMap::new();
    extract.insert("token".to_owned(), "$.data.token".to_owned());
    let with_rules = create_endpoint(root, "With Rules").unwrap();
    save_endpoint(
        &with_rules,
        &Endpoint {
            seq: 0,
            name: "With Rules".to_owned(),
            assertions: Vec::new(),
            extract,
            persist: vec!["token".to_owned()],
            request: req(),
        },
    )
    .unwrap();

    // Rule-free twin hitting the same URL.
    let no_rules = create_endpoint(root, "No Rules").unwrap();
    save_endpoint(
        &no_rules,
        &Endpoint {
            seq: 0,
            name: "No Rules".to_owned(),
            assertions: Vec::new(),
            extract: BTreeMap::new(),
            persist: Vec::new(),
            request: req(),
        },
    )
    .unwrap();

    let with = churl_in(root, &["--json", "run", "With Rules"]);
    let without = churl_in(root, &["--json", "run", "No Rules"]);
    assert!(
        with.status.success() && without.status.success(),
        "stderr: {} / {}",
        String::from_utf8_lossy(&with.stderr),
        String::from_utf8_lossy(&without.stderr)
    );

    let ew = envelope(&with);
    let eo = envelope(&without);
    assert_eq!(ew["schema_version"], 1, "schema_version stays frozen at 1");
    assert_eq!(ew["command"], "run");
    // The rule-bearing endpoint's response envelope matches the rule-free twin:
    // extraction never runs headless, so nothing changes and nothing is captured.
    assert_eq!(ew["data"]["response"]["status"], 200);
    // Compare the deterministic response fields (timing_ms and the Date header
    // vary run-to-run): the rule-bearing and rule-free envelopes must match.
    for field in ["status", "body", "body_encoding", "truncated"] {
        assert_eq!(
            ew["data"]["response"][field], eo["data"]["response"][field],
            "extract rules must not alter the headless response {field}"
        );
    }
    // No extraction/session artifact leaks into the headless `data` object.
    let data = ew["data"].as_object().expect("data is an object");
    for forbidden in ["extract", "persist", "captured", "session"] {
        assert!(
            !data.contains_key(forbidden),
            "headless data must not carry a {forbidden:?} key: {ew}"
        );
    }
}
