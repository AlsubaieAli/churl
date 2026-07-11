//! Integration tests for the M7.4 request-sequence run engine: multi-step
//! extraction chains asserted on the wire (`wiremock`), the `on_error`
//! halt/continue policy, extracted-var precedence, path-traversal rejection, and
//! sequence TOML round-trip stability over a comment corpus.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use churl_core::http::{DEFAULT_TIMEOUT, ExecuteOptions, build_client};
use churl_core::model::{
    Body, BodyKind, Endpoint, Header, Method, OnError, Request, Sequence, SequenceStep,
};
use churl_core::persistence::{
    OpenWorkspace, load_sequence, save_endpoint, save_sequence, save_workspace_manifest,
};
use churl_core::sequence::{RunScopes, StepResult, prepare_step, run_sequence};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, ResponseTemplate};

/// A minimal GET endpoint at `url`.
fn get_endpoint(name: &str, url: &str) -> Endpoint {
    Endpoint {
        seq: 0,
        name: name.to_owned(),
        request: Request {
            method: Method::Get,
            url: url.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
        },
    }
}

/// Writes an endpoint into `<root>/<collection>/<file>` (creating the collection
/// dir), returning nothing — the sequence step refers to it by relative path.
fn write_endpoint(root: &Path, rel: &str, endpoint: &Endpoint) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    save_endpoint(&path, endpoint).unwrap();
}

/// A bare workspace manifest so `OpenWorkspace::open` works.
fn write_manifest(root: &Path, vars: &[(&str, &str)]) {
    let ws = churl_core::model::Workspace {
        name: "test-ws".into(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        profiles: Vec::new(),
    };
    save_workspace_manifest(root, &ws).unwrap();
}

fn step(endpoint: &str, extract: &[(&str, &str)]) -> SequenceStep {
    SequenceStep {
        seq: 0,
        endpoint: endpoint.to_owned(),
        extract: extract
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        persist: Vec::new(),
    }
}

/// The headline test: a 3-step chain where step 1 extracts a token (used in step
/// 2's header) and an id (used in step 3's URL), asserted on the wire.
#[tokio::test]
async fn three_step_chain_feeds_extracted_values_downstream() {
    let server = MockServer::start().await;

    // Step 1: login returns a token + user id.
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"data":{"token":"TOK123","user":{"id":42}}}"#),
        )
        .mount(&server)
        .await;

    // Step 2: /me must arrive with Authorization: Bearer TOK123 (the extracted token).
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer TOK123"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    // Step 3: /users/42 — the extracted id substituted into the URL path.
    Mock::given(method("GET"))
        .and(path("/users/42"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);

    write_endpoint(
        root,
        "auth/login.toml",
        &get_endpoint("login", &format!("{}/login", server.uri())),
    );
    // Step 2 uses {{token}} in an Authorization header.
    let mut me = get_endpoint("me", &format!("{}/me", server.uri()));
    me.request.headers.push(Header {
        name: "Authorization".into(),
        value: "Bearer {{token}}".into(),
        enabled: true,
    });
    write_endpoint(root, "users/me.toml", &me);
    // Step 3 uses {{user_id}} in the URL.
    write_endpoint(
        root,
        "users/by_id.toml",
        &get_endpoint("by_id", &format!("{}/users/{{{{user_id}}}}", server.uri())),
    );

    let sequence = Sequence {
        seq: 0,
        name: "auth flow".into(),
        on_error: OnError::Halt,
        steps: vec![
            step(
                "auth/login.toml",
                &[("token", "$.data.token"), ("user_id", "$.data.user.id")],
            ),
            step("users/me.toml", &[]),
            step("users/by_id.toml", &[]),
        ],
    };

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(run.steps.len(), 3);
    assert_eq!(run.steps[0].result, StepResult::Ok { status: 200 });
    assert_eq!(run.steps[0].extracted.get("token").unwrap(), "TOK123");
    assert_eq!(run.steps[0].extracted.get("user_id").unwrap(), "42");
    assert_eq!(run.steps[1].result, StepResult::Ok { status: 200 });
    assert_eq!(run.steps[2].result, StepResult::Ok { status: 200 });
    // The mocks with matchers would not have responded 200 unless the extracted
    // values reached the wire; assert the requests actually landed.
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().any(|r| r.url.path() == "/users/42"));
    assert!(requests.iter().any(|r: &WmRequest| r.url.path() == "/me"
        && r.headers.get("authorization").map(|v| v.to_str().unwrap()) == Some("Bearer TOK123")));
}

#[tokio::test]
async fn halt_on_failure_skips_the_rest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/boom"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    write_endpoint(
        root,
        "c/ok.toml",
        &get_endpoint("ok", &format!("{}/ok", server.uri())),
    );
    write_endpoint(
        root,
        "c/boom.toml",
        &get_endpoint("boom", &format!("{}/boom", server.uri())),
    );
    write_endpoint(
        root,
        "c/never.toml",
        &get_endpoint("never", &format!("{}/never", server.uri())),
    );

    let sequence = Sequence {
        seq: 0,
        name: "halt".into(),
        on_error: OnError::Halt,
        steps: vec![
            step("c/ok.toml", &[]),
            step("c/boom.toml", &[]),
            step("c/never.toml", &[]),
        ],
    };

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(run.steps[0].result, StepResult::Ok { status: 200 });
    assert_eq!(run.steps[1].result, StepResult::Failed { status: 500 });
    assert_eq!(run.steps[2].result, StepResult::Skipped);
    // The skipped step never hit the wire.
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.url.path() == "/never"));
}

#[tokio::test]
async fn continue_on_failure_runs_the_rest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/boom"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/after"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    write_endpoint(
        root,
        "c/boom.toml",
        &get_endpoint("boom", &format!("{}/boom", server.uri())),
    );
    write_endpoint(
        root,
        "c/after.toml",
        &get_endpoint("after", &format!("{}/after", server.uri())),
    );

    let sequence = Sequence {
        seq: 0,
        name: "continue".into(),
        on_error: OnError::Continue,
        steps: vec![step("c/boom.toml", &[]), step("c/after.toml", &[])],
    };

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(run.steps[0].result, StepResult::Failed { status: 500 });
    assert_eq!(run.steps[1].result, StepResult::Ok { status: 200 });
}

#[tokio::test]
async fn extraction_error_halts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"present":1}"#))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    write_endpoint(
        root,
        "c/first.toml",
        &get_endpoint("first", &format!("{}/first", server.uri())),
    );
    write_endpoint(
        root,
        "c/second.toml",
        &get_endpoint("second", &format!("{}/second", server.uri())),
    );

    let sequence = Sequence {
        seq: 0,
        name: "extract-halt".into(),
        on_error: OnError::Halt,
        // Extract a key that does not exist → the step fails on extraction.
        steps: vec![
            step("c/first.toml", &[("x", "$.absent")]),
            step("c/second.toml", &[]),
        ],
    };

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;

    assert!(matches!(run.steps[0].result, StepResult::ExtractError(_)));
    assert!(run.steps[0].extracted.is_empty());
    assert_eq!(run.steps[1].result, StepResult::Skipped);
}

/// Extracted values must beat an ambient workspace var of the same name — a
/// chained value is never shadowed by ambient config.
#[tokio::test]
async fn extracted_var_beats_workspace_var() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"region":"FRESH"}"#))
        .mount(&server)
        .await;
    // Step 2 succeeds only if it sends the *extracted* region, not the workspace one.
    Mock::given(method("GET"))
        .and(path("/guarded"))
        .and(header("x-region", "FRESH"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A workspace var `region` (non-secret) that must lose to the extracted one.
    write_manifest(root, &[("region", "STALE")]);
    write_endpoint(
        root,
        "c/first.toml",
        &get_endpoint("first", &format!("{}/first", server.uri())),
    );
    let mut guarded = get_endpoint("guarded", &format!("{}/guarded", server.uri()));
    guarded.request.headers.push(Header {
        name: "X-Region".into(),
        value: "{{region}}".into(),
        enabled: true,
    });
    write_endpoint(root, "c/guarded.toml", &guarded);

    let sequence = Sequence {
        seq: 0,
        name: "precedence".into(),
        on_error: OnError::Halt,
        steps: vec![
            step("c/first.toml", &[("region", "$.region")]),
            step("c/guarded.toml", &[]),
        ],
    };

    let scopes = RunScopes {
        workspace: [("region".to_string(), "STALE".to_string())]
            .into_iter()
            .collect(),
        ..RunScopes::default()
    };

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &scopes,
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(run.steps[0].extracted.get("region").unwrap(), "FRESH");
    assert_eq!(
        run.steps[1].result,
        StepResult::Ok { status: 200 },
        "step 2 must have sent the extracted region FRESH, not STALE"
    );
}

#[test]
fn prepare_step_rejects_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    let escaping = step("../escape.toml", &[]);
    let err = prepare_step(root, &escaping, &BTreeMap::new(), &RunScopes::default());
    assert!(err.is_err(), "traversal endpoint must be rejected");
}

#[test]
fn prepare_step_fails_loud_on_unresolved_variable() {
    // A step whose request has a `{{var}}` no scope resolves must fail at prepare
    // time (fail loud) rather than shipping a literal `{{echoed}}` on the wire. The
    // error names the variable so the message is actionable.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    write_endpoint(
        root,
        "c/echo.toml",
        &get_endpoint("echo", "https://httpbin.test/get?e={{echoed}}"),
    );
    let s = step("c/echo.toml", &[]);
    let err = prepare_step(root, &s, &BTreeMap::new(), &RunScopes::default())
        .expect_err("unresolved variable must fail the prepare step");
    let msg = err.to_string();
    assert!(
        msg.contains("unresolved variable(s): echoed"),
        "error must name the unresolved variable, got: {msg}"
    );
}

#[test]
fn prepare_step_resolves_variable_from_scope_and_succeeds() {
    // The success path is unchanged: when a scope resolves the variable, prepare
    // returns the substituted request with no leftover placeholder.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[("echoed", "hello")]);
    write_endpoint(
        root,
        "c/echo.toml",
        &get_endpoint("echo", "https://httpbin.test/get?e={{echoed}}"),
    );
    let s = step("c/echo.toml", &[]);
    let scopes = RunScopes {
        workspace: [("echoed".to_string(), "hello".to_string())]
            .into_iter()
            .collect(),
        ..RunScopes::default()
    };
    let prepared = prepare_step(root, &s, &BTreeMap::new(), &scopes)
        .expect("a resolved variable must prepare cleanly");
    assert_eq!(prepared.url, "https://httpbin.test/get?e=hello");
}

#[test]
fn prepare_step_missing_endpoint_errors_without_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    let missing = step("c/nope.toml", &[]);
    assert!(prepare_step(root, &missing, &BTreeMap::new(), &RunScopes::default()).is_err());
}

/// A `sequences/` directory must not surface as a collection.
#[test]
fn sequences_dir_is_not_a_collection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    fs::create_dir_all(root.join("sequences")).unwrap();
    fs::create_dir_all(root.join("real-collection")).unwrap();

    let ws = OpenWorkspace::open(root).unwrap();
    let collections = ws.collections().unwrap();
    assert!(
        collections.iter().all(|c| c.name != "sequences"),
        "sequences/ must be excluded from collections"
    );
    assert!(collections.iter().any(|c| c.name == "real-collection"));
}

/// A single unparseable sequence file degrades to a warning, never aborts.
#[test]
fn one_bad_sequence_file_degrades_to_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    let dir = root.join("sequences");
    fs::create_dir_all(&dir).unwrap();
    // A valid one and a broken one.
    save_sequence(
        &dir.join("good.toml"),
        &Sequence {
            seq: 0,
            name: "good".into(),
            on_error: OnError::Halt,
            steps: vec![],
        },
    )
    .unwrap();
    fs::write(dir.join("bad.toml"), "this = is = not valid toml =").unwrap();

    let ws = OpenWorkspace::open(root).unwrap();
    let load = ws.sequences().unwrap();
    assert_eq!(load.sequences.len(), 1);
    assert_eq!(load.sequences[0].1.name, "good");
    assert_eq!(load.warnings.len(), 1);
    assert!(load.warnings[0].contains("bad.toml"));
}

/// Sequence TOML with hand-written comments must survive a load → save → reload
/// byte-for-byte (format-preserving merge).
#[test]
fn sequence_toml_round_trip_is_byte_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth-flow.toml");
    let original = r#"seq = 0
name = "Auth flow"
on_error = "halt"

# The login step — grabs a token and the user id.
[[step]]
seq = 0
endpoint = "auth/login.toml"
[step.extract]
token = "$.data.token"
user_id = "$.data.user.id"

# Uses {{token}} in its Authorization header.
[[step]]
seq = 1
endpoint = "users/me.toml"
"#;
    fs::write(&path, original).unwrap();

    let sequence = load_sequence(&path).unwrap();
    save_sequence(&path, &sequence).unwrap();
    let reloaded = fs::read_to_string(&path).unwrap();
    assert_eq!(reloaded, original, "round-trip must be byte-stable");

    // And the parsed structure is what we expect.
    assert_eq!(sequence.name, "Auth flow");
    assert_eq!(sequence.on_error, OnError::Halt);
    assert_eq!(sequence.steps.len(), 2);
    assert_eq!(sequence.steps[0].endpoint, "auth/login.toml");
    assert_eq!(
        sequence.steps[0].extract.get("token").unwrap(),
        "$.data.token"
    );
}

/// A fresh save (no existing file) produces the documented on-disk shape and
/// parses back into an equal value.
#[test]
fn sequence_fresh_save_shape_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.toml");
    let sequence = Sequence {
        seq: 3,
        name: "My seq".into(),
        on_error: OnError::Continue,
        steps: vec![
            step("a/one.toml", &[("id", "$.id")]),
            step("b/two.toml", &[]),
        ],
    };
    save_sequence(&path, &sequence).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("on_error = \"continue\""), "{text}");
    assert!(text.contains("[[step]]"), "{text}");
    assert!(text.contains("[step.extract]"), "{text}");

    let reloaded = load_sequence(&path).unwrap();
    assert_eq!(reloaded, sequence);
}

/// Note #6: the `persist` name list serializes as `persist = [...]` and
/// round-trips (load → toggle a persist name → save → reload → identical).
#[test]
fn persist_field_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.toml");
    let mut sequence = Sequence {
        seq: 0,
        name: "auth".into(),
        on_error: OnError::Halt,
        steps: vec![step("auth/login.toml", &[("token", "$.token")])],
    };
    sequence.steps[0].persist = vec!["token".to_owned()];
    save_sequence(&path, &sequence).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("persist = [\"token\"]"), "{text}");

    // Reload is equal, then toggle the persist name off and back on.
    let mut reloaded = load_sequence(&path).unwrap();
    assert_eq!(reloaded, sequence);
    reloaded.steps[0].persist.clear();
    save_sequence(&path, &reloaded).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("persist"),
        "empty persist is omitted:\n{text}"
    );
    let empty = load_sequence(&path).unwrap();
    assert!(empty.steps[0].persist.is_empty());
    // Re-add it; the name comes back verbatim.
    let mut readd = empty;
    readd.steps[0].persist = vec!["token".to_owned()];
    save_sequence(&path, &readd).unwrap();
    assert_eq!(load_sequence(&path).unwrap(), readd);
}

/// Backward-compat: a sequence file with no `persist` key loads with every rule
/// Run-only (empty `persist`).
#[test]
fn missing_persist_loads_as_run_only() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("legacy.toml");
    let legacy = r#"seq = 0
name = "legacy"
on_error = "halt"

[[step]]
seq = 0
endpoint = "auth/login.toml"
[step.extract]
token = "$.token"
"#;
    fs::write(&path, legacy).unwrap();
    let sequence = load_sequence(&path).unwrap();
    assert_eq!(sequence.steps.len(), 1);
    assert!(
        sequence.steps[0].persist.is_empty(),
        "old files → all rules Run-only"
    );
    assert!(sequence.steps[0].extract.contains_key("token"));
}

/// Format-preserving: toggling a persist name on one step keeps hand-written
/// comments on the *sibling* step intact.
#[test]
fn toggling_persist_preserves_sibling_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("auth-flow.toml");
    let original = r#"seq = 0
name = "Auth flow"
on_error = "halt"

# The login step — grabs a token.
[[step]]
seq = 0
endpoint = "auth/login.toml"
[step.extract]
token = "$.data.token"

# Uses {{token}} in its Authorization header.
[[step]]
seq = 1
endpoint = "users/me.toml"
"#;
    fs::write(&path, original).unwrap();

    let mut sequence = load_sequence(&path).unwrap();
    // Mark step 0's `token` rule as a Session target.
    sequence.steps[0].persist = vec!["token".to_owned()];
    save_sequence(&path, &sequence).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    // The new field landed…
    assert!(text.contains("persist = [\"token\"]"), "{text}");
    // …and both comments survive, including the sibling step's.
    assert!(text.contains("# The login step — grabs a token."), "{text}");
    assert!(
        text.contains("# Uses {{token}} in its Authorization header."),
        "sibling comment preserved:\n{text}"
    );
    // The sibling step (no persist) is unchanged.
    let reloaded = load_sequence(&path).unwrap();
    assert!(reloaded.steps[1].persist.is_empty());
    assert_eq!(reloaded.steps[1].endpoint, "users/me.toml");
}

/// An empty-step sequence must not panic and yields no outcomes.
#[tokio::test]
async fn empty_sequence_produces_no_outcomes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    let sequence = Sequence {
        seq: 0,
        name: "empty".into(),
        on_error: OnError::Halt,
        steps: vec![],
    };
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;
    assert!(run.steps.is_empty());
}

/// A step whose endpoint sends a body still runs (guards the body path through the
/// shared prepare→execute chain).
#[tokio::test]
async fn step_with_body_runs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/create"))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id":"new"}"#))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_manifest(root, &[]);
    let mut ep = get_endpoint("create", &format!("{}/create", server.uri()));
    ep.request.method = Method::Post;
    ep.request.body = Some(Body {
        kind: BodyKind::Json,
        content: r#"{"name":"x"}"#.into(),
    });
    write_endpoint(root, "c/create.toml", &ep);

    let sequence = Sequence {
        seq: 0,
        name: "post".into(),
        on_error: OnError::Halt,
        steps: vec![step("c/create.toml", &[("id", "$.id")])],
    };
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let run = run_sequence(
        &client,
        root,
        &sequence,
        &RunScopes::default(),
        &ExecuteOptions::default(),
    )
    .await;
    assert_eq!(run.steps[0].result, StepResult::Ok { status: 201 });
    assert_eq!(run.steps[0].extracted.get("id").unwrap(), "new");
}
