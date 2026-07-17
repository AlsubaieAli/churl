//! Integration tests for `churl send` — the M8.2 headless ad-hoc request path.
//! Drives the real binary against an in-process `wiremock` server (no real
//! network) and asserts on the frozen envelope bytes + exit codes, not just
//! substrings of human text.

use std::process::{Command, Output};

use serde_json::Value;
use wiremock::matchers::{body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn churl(args: &[&str]) -> Output {
    // Isolate the global config so a real user config on the test box
    // (proxy/insecure/timeout overrides) can never skew these assertions. The
    // path must be a plain nonexistent file (not e.g. under a non-directory)
    // so `load_config` sees a clean `NotFound` and falls back to
    // `Config::default()` rather than a hard read error.
    let missing_config = std::env::temp_dir().join(format!(
        "churl-cli-send-test-nonexistent-config-{}.toml",
        std::process::id()
    ));
    churl_with_config(args, &missing_config)
}

/// Runs the binary with `CHURL_CONFIG` pinned to `config` — lets a test point
/// at a deliberately corrupt config file to exercise the pre-flight failure
/// surface.
fn churl_with_config(args: &[&str], config: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .env("CHURL_CONFIG", config)
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

#[tokio::test]
async fn send_json_envelope_shape_on_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;

    let url = format!("{}/ping", server.uri());
    let output = churl(&["--json", "send", &url]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr must be silent in --json mode"
    );

    let env = envelope(&output);
    assert_eq!(env["schema_version"], 1);
    assert_eq!(env["ok"], true);
    assert_eq!(env["command"], "send");
    assert!(env["error"].is_null());
    assert_eq!(env["data"]["request"]["method"], "GET");
    assert_eq!(env["data"]["request"]["url"], url);
    assert_eq!(env["data"]["request"]["body_present"], false);
    assert_eq!(env["data"]["response"]["status"], 200);
    assert_eq!(env["data"]["response"]["body"], "pong");
    assert_eq!(env["data"]["response"]["body_encoding"], "utf8");
    assert_eq!(env["data"]["response"]["truncated"], false);
    assert!(env["data"]["response"]["timing_ms"]["total"].is_u64());
    // RESERVED for M8.4 — shipped as `null` now so the key never bumps schema_version later.
    assert!(env["data"]["assertions"].is_null());
}

#[tokio::test]
async fn send_curl_mnemonic_and_churl_native_flags_are_equivalent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/widgets"))
        .and(header("content-type", "application/json"))
        .and(body_string(r#"{"n":1}"#))
        .respond_with(ResponseTemplate::new(201).set_body_string("created"))
        .mount(&server)
        .await;

    let url = format!("{}/widgets", server.uri());

    // curl-mnemonic form.
    let out1 = churl(&[
        "--json",
        "send",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"n":1}"#,
        &url,
    ]);
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert_eq!(envelope(&out1)["data"]["response"]["status"], 201);

    // churl-native form.
    let out2 = churl(&[
        "--json",
        "send",
        "--method",
        "POST",
        "--header",
        "Content-Type: application/json",
        "--body",
        r#"{"n":1}"#,
        "--url",
        &url,
    ]);
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert_eq!(envelope(&out2)["data"]["response"]["status"], 201);
}

#[tokio::test]
async fn send_body_without_explicit_method_defaults_to_post_like_curl() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/echo", server.uri());

    let output = churl(&["--json", "send", "-d", "hello", &url]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(envelope(&output)["data"]["request"]["method"], "POST");
}

#[tokio::test]
async fn send_no_method_no_body_defaults_to_get() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plain"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/plain", server.uri());

    let output = churl(&["--json", "send", &url]);
    assert!(output.status.success());
    assert_eq!(envelope(&output)["data"]["request"]["method"], "GET");
}

#[tokio::test]
async fn send_masks_auth_bearing_header_in_the_echoed_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/secure"))
        // The REAL request must still carry the real header value.
        .and(header("authorization", "Bearer real-secret-value"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/secure", server.uri());

    let output = churl(&[
        "--json",
        "send",
        "-H",
        "Authorization: Bearer real-secret-value",
        &url,
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    // wiremock only matched if the real header went out — status 200 proves it.
    assert_eq!(env["data"]["response"]["status"], 200);
    // But the EECHOED request headers must never show the real value.
    let headers = env["data"]["request"]["headers"].as_array().unwrap();
    let auth = headers
        .iter()
        .find(|h| h["name"] == "Authorization")
        .expect("Authorization header echoed");
    assert_ne!(auth["value"], "Bearer real-secret-value");
    assert!(
        !auth["value"]
            .as_str()
            .unwrap()
            .contains("real-secret-value")
    );
}

#[tokio::test]
async fn send_base64_encodes_non_utf8_response_bodies() {
    let server = MockServer::start().await;
    let bytes: Vec<u8> = vec![0xff, 0xfe, 0x00, 0xff, 0xd8, 0xff];
    Mock::given(method("GET"))
        .and(path("/binary"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    let url = format!("{}/binary", server.uri());

    let output = churl(&["--json", "send", &url]);
    assert!(output.status.success());
    let env = envelope(&output);
    assert_eq!(env["data"]["response"]["body_encoding"], "base64");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(env["data"]["response"]["body"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, bytes);
}

#[test]
fn send_invalid_url_is_exit_4_invalid_url() {
    let output = churl(&["--json", "send", "not-a-url-at-all"]);
    assert_eq!(output.status.code(), Some(4));
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "invalid-url");
}

#[test]
fn send_connection_refused_is_exit_4_transport_error() {
    // Port 1 is a privileged port essentially never listening in test sandboxes;
    // the connect attempt fails fast with ECONNREFUSED (or times out, still a
    // transport failure either way).
    let output = churl(&["--json", "send", "http://127.0.0.1:1/"]);
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(4));
    let env = envelope(&output);
    assert_eq!(env["ok"], false);
    assert!(matches!(
        env["error"]["kind"].as_str(),
        Some("transport-error") | Some("timeout")
    ));
}

#[test]
fn send_missing_url_is_a_clap_usage_error_exit_2() {
    // Band 2 is owned entirely by clap — no JSON envelope even with --json,
    // per the frozen contract ("clap default — don't remap").
    let output = churl(&["--json", "send"]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "clap usage errors print to stderr, not stdout"
    );
}

#[tokio::test]
async fn send_human_mode_prints_body_on_stdout() {
    // No --json: human mode prints the raw response body on stdout (curl-like,
    // so `churl send URL | jq` works).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("the-body"))
        .mount(&server)
        .await;
    let url = format!("{}/echo", server.uri());

    let output = churl(&["send", &url]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "the-body");
}

// --- P0-1 pre-flight failure surface (regression) ---

#[test]
fn send_corrupt_global_config_is_exit_3_config_error_not_exit_1() {
    // A malformed global config must surface a band-3 envelope, NEVER an exit-1
    // anyhow bubble (exit 1 is RESERVED for assertion failure) with empty
    // stdout under --json.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "redirect = \"not-a-valid-policy\"\n").unwrap();

    let output = churl_with_config(&["--json", "send", "http://127.0.0.1:1/"], &cfg);
    assert_eq!(
        output.status.code(),
        Some(3),
        "must be band 3, never exit 1"
    );
    let env = envelope(&output); // asserts stdout carries a valid, non-empty envelope
    assert_eq!(env["ok"], false);
    assert!(env["data"].is_null());
    assert_eq!(env["error"]["kind"], "config-error");
}

#[test]
fn send_malformed_var_is_a_clap_usage_error_exit_2() {
    // A `--var` without `=` is a clap value-parser failure → band 2, envelope-
    // exempt, never bubbles out of main as exit 1.
    let output = churl(&["--json", "send", "--var", "noequals", "http://127.0.0.1:1/"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "clap usage errors print to stderr, not stdout"
    );
}

// --- P0-2 request.url masking (success surface) ---

#[tokio::test]
async fn send_masks_userinfo_password_in_echoed_request_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    // Inject `admin:s3cr3t@` userinfo into the wiremock URL.
    let host_port = server.uri().strip_prefix("http://").unwrap().to_owned();
    let url = format!("http://admin:s3cr3t@{host_port}/x");

    let output = churl(&["--json", "send", &url]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    let echoed = env["data"]["request"]["url"].as_str().unwrap();
    assert!(
        !echoed.contains("s3cr3t"),
        "password leaked in url: {echoed}"
    );
    assert!(
        echoed.contains("admin"),
        "username should stay visible: {echoed}"
    );
}

#[tokio::test]
async fn send_masks_userinfo_password_in_verbose_stderr_trace() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let host_port = server.uri().strip_prefix("http://").unwrap().to_owned();
    let url = format!("http://admin:s3cr3t@{host_port}/x");

    // Human mode + -v prints the request line on stderr; it must be masked too.
    let output = churl(&["send", "-v", &url]);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("s3cr3t"),
        "password leaked in -v trace: {stderr}"
    );
}

// --- P0-3 request.url masking (failure surface) ---

#[test]
fn send_invalid_url_masks_userinfo_in_error_message_and_detail() {
    // A non-numeric port makes reqwest's URL parse fail (InvalidUrl); the URL
    // still carries a `user:pass@` password that must not leak into either the
    // human message or the structured detail.
    let output = churl(&[
        "--json",
        "send",
        "http://admin:s3cr3t@host.example.com:notaport/x",
    ]);
    assert_eq!(output.status.code(), Some(4));
    let env = envelope(&output);
    assert_eq!(env["error"]["kind"], "invalid-url");
    let message = env["error"]["message"].as_str().unwrap();
    let detail_url = env["error"]["detail"]["url"].as_str().unwrap();
    assert!(!message.contains("s3cr3t"), "leaked in message: {message}");
    assert!(
        !detail_url.contains("s3cr3t"),
        "leaked in detail.url: {detail_url}"
    );
}

#[test]
fn send_transport_error_masks_secret_query_in_error_message_and_detail() {
    // Connection-refused (port 1) is the *common* failure mode. reqwest's error
    // `Display` embeds the full URL — query string included — so a secret-named
    // `?api_key=…` must be masked on the transport-error surface exactly as it
    // is on the success and InvalidUrl surfaces. Guards the gap where the fix
    // originally masked only the InvalidUrl arm.
    let output = churl(&[
        "--json",
        "send",
        "http://127.0.0.1:1/x?api_key=REALKEY123abc",
    ]);
    assert_eq!(output.status.code(), Some(4));
    let env = envelope(&output);
    assert_eq!(env["error"]["kind"], "transport-error");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("REALKEY123abc"),
        "secret leaked in transport error message: {message}"
    );
    if let Some(detail_url) = env["error"]["detail"]["url"].as_str() {
        assert!(
            !detail_url.contains("REALKEY123abc"),
            "secret leaked in transport error detail.url: {detail_url}"
        );
    }
}
