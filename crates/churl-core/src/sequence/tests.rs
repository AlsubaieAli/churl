use super::*;
use crate::model::{Header, Timing};
use std::time::Duration;

fn json_response(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![Header {
            name: "Content-Type".into(),
            value: "application/json".into(),
            enabled: true,
        }],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(1),
        },
    }
}

#[test]
fn extract_status() {
    let mut response = json_response("{}");
    response.status = 201;
    assert_eq!(extract_value(&response, "status").unwrap(), "201");
}

#[test]
fn extract_header_case_insensitive() {
    let response = json_response("{}");
    assert_eq!(
        extract_value(&response, "header:content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        extract_value(&response, "header:Content-Type").unwrap(),
        "application/json"
    );
}

#[test]
fn extract_missing_header_errors() {
    let response = json_response("{}");
    assert!(matches!(
        extract_value(&response, "header:X-Absent"),
        Err(ExtractError::HeaderMissing { .. })
    ));
}

#[test]
fn extract_nested_path_optional_dollar() {
    let response = json_response(r#"{"data":{"token":"abc","user":{"id":7}}}"#);
    assert_eq!(extract_value(&response, "$.data.token").unwrap(), "abc");
    assert_eq!(extract_value(&response, "data.token").unwrap(), "abc");
    assert_eq!(extract_value(&response, "$.data.user.id").unwrap(), "7");
}

#[test]
fn extract_array_indices() {
    let response = json_response(r#"{"items":[{"id":10},{"id":20}]}"#);
    assert_eq!(extract_value(&response, "$.items[0].id").unwrap(), "10");
    assert_eq!(extract_value(&response, "items[1].id").unwrap(), "20");
}

#[test]
fn extract_typed_leaves() {
    let response = json_response(r#"{"n":42,"f":1.5,"b":true,"s":"hi","o":{"k":1},"a":[1,2]}"#);
    assert_eq!(extract_value(&response, "$.n").unwrap(), "42");
    assert_eq!(extract_value(&response, "$.f").unwrap(), "1.5");
    assert_eq!(extract_value(&response, "$.b").unwrap(), "true");
    assert_eq!(extract_value(&response, "$.s").unwrap(), "hi");
    // Object and array leaves become compact JSON.
    assert_eq!(extract_value(&response, "$.o").unwrap(), r#"{"k":1}"#);
    assert_eq!(extract_value(&response, "$.a").unwrap(), "[1,2]");
}

#[test]
fn extract_null_leaf_errors() {
    let response = json_response(r#"{"x":null}"#);
    assert!(matches!(
        extract_value(&response, "$.x"),
        Err(ExtractError::NullLeaf { .. })
    ));
}

#[test]
fn extract_missing_key_and_bad_index() {
    let response = json_response(r#"{"a":[1]}"#);
    assert!(matches!(
        extract_value(&response, "$.nope"),
        Err(ExtractError::MissingKey { .. })
    ));
    assert!(matches!(
        extract_value(&response, "$.a[5]"),
        Err(ExtractError::IndexOutOfRange { len: 1, .. })
    ));
}

#[test]
fn extract_type_mismatch() {
    let response = json_response(r#"{"a":"str"}"#);
    // Indexing a string, or keying an array.
    assert!(matches!(
        extract_value(&response, "$.a[0]"),
        Err(ExtractError::TypeMismatch { .. })
    ));
    let response = json_response(r#"{"a":[1,2]}"#);
    assert!(matches!(
        extract_value(&response, "$.a.foo"),
        Err(ExtractError::TypeMismatch { .. })
    ));
}

#[test]
fn extract_non_json_body_errors() {
    let mut response = json_response("");
    response.body = b"<html>not json</html>".to_vec();
    assert!(matches!(
        extract_value(&response, "$.a"),
        Err(ExtractError::NotJson { .. })
    ));
}

#[test]
fn extract_malformed_expr_errors() {
    let response = json_response(r#"{"a":1}"#);
    for expr in ["$.", "a..b", "a[", "a[x]", "$"] {
        assert!(
            matches!(
                extract_value(&response, expr),
                Err(ExtractError::BadExpr { .. })
            ),
            "expected BadExpr for {expr:?}"
        );
    }
}

#[test]
fn extract_unicode_key() {
    let response = json_response(r#"{"café":{"naïve":"yes"}}"#);
    assert_eq!(extract_value(&response, "café.naïve").unwrap(), "yes");
}

#[test]
fn parse_path_shapes() {
    assert_eq!(
        parse_path("$.a.b[0].c").unwrap(),
        vec![
            PathSeg::Key("a".into()),
            PathSeg::Key("b".into()),
            PathSeg::Index(0),
            PathSeg::Key("c".into()),
        ]
    );
    assert_eq!(
        parse_path("a.b[2]").unwrap(),
        vec![
            PathSeg::Key("a".into()),
            PathSeg::Key("b".into()),
            PathSeg::Index(2),
        ]
    );
}

#[test]
fn resolve_step_path_rejects_traversal() {
    // Lexical rejections work even for a non-existent root (no canonicalize).
    let root = Path::new("/ws");
    assert!(matches!(
        resolve_step_path(root, "../secret.toml"),
        Err(SequenceError::Traversal { .. })
    ));
    assert!(matches!(
        resolve_step_path(root, "a/../../secret.toml"),
        Err(SequenceError::Traversal { .. })
    ));
    assert!(matches!(
        resolve_step_path(root, "/etc/passwd"),
        Err(SequenceError::Traversal { .. })
    ));
}

#[test]
fn resolve_step_path_accepts_in_workspace_path() {
    // A normal relative path inside a real workspace resolves cleanly, whether
    // or not the file exists yet.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::write(root.join("auth/login.toml"), "x").unwrap();
    assert_eq!(
        resolve_step_path(root, "auth/login.toml").unwrap(),
        root.join("auth/login.toml")
    );
    // A missing file is NOT a traversal (its ancestor is inside root); the
    // load step later reports not-found — a distinct failure kind.
    assert!(resolve_step_path(root, "auth/missing.toml").is_ok());
}

#[cfg(unix)]
#[test]
fn resolve_step_path_rejects_symlink_escape() {
    // An in-workspace symlink dir pointing outside the root must not let a step
    // escape — the lexical check misses this; canonical containment catches it.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.toml"), "seq=0\nname=\"s\"").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let root = ws.path();
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    // The symlinked file really exists (so canonicalize resolves it) yet lands
    // outside the root → Traversal.
    assert!(matches!(
        resolve_step_path(root, "link/secret.toml"),
        Err(SequenceError::Traversal { .. })
    ));
    // prepare_step surfaces the same rejection (never loads the outside file).
    let step = SequenceStep {
        seq: 0,
        endpoint: "link/secret.toml".into(),
        extract: BTreeMap::new(),
        persist: Vec::new(),
    };
    assert!(matches!(
        prepare_step(root, &step, &BTreeMap::new(), &RunScopes::default()),
        Err(SequenceError::Traversal { .. })
    ));
}

#[test]
fn classify_step_variants() {
    let step = SequenceStep {
        seq: 0,
        endpoint: "e.toml".into(),
        extract: BTreeMap::new(),
        persist: Vec::new(),
    };
    // Ok
    let (result, _) = classify_step(&Ok(json_response("{}")), &step);
    assert_eq!(result, StepResult::Ok { status: 200 });
    // Failed (>= 400)
    let mut bad = json_response("{}");
    bad.status = 500;
    let (result, _) = classify_step(&Ok(bad), &step);
    assert_eq!(result, StepResult::Failed { status: 500 });
    assert!(result.is_failure());
    // HttpError
    let (result, _) = classify_step(&Err(HttpError::Timeout), &step);
    assert!(matches!(result, StepResult::HttpError(_)));
}

#[test]
fn should_halt_only_on_failure_under_halt() {
    assert!(should_halt(
        &StepResult::Failed { status: 500 },
        OnError::Halt
    ));
    assert!(should_halt(
        &StepResult::HttpError("x".into()),
        OnError::Halt
    ));
    assert!(should_halt(
        &StepResult::ExtractError("x".into()),
        OnError::Halt
    ));
    assert!(!should_halt(&StepResult::Ok { status: 200 }, OnError::Halt));
    // Continue never halts, even on failure.
    assert!(!should_halt(
        &StepResult::Failed { status: 500 },
        OnError::Continue
    ));
}

#[test]
fn prepare_step_session_scope_precedence() {
    // Note #6: the run scope chain is
    // `extracted > session > cli > profile > collection > workspace`. This
    // exercises the ordering directly through prepare_step by giving the same
    // var name a value in adjacent layers and asserting the resolved URL.
    use crate::model::{Body, BodyKind, Endpoint, Method, Request};
    use crate::persistence::save_endpoint;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("c")).unwrap();
    // An endpoint whose URL is just `{{x}}` so the resolved URL == the winner.
    let ep = Endpoint {
        seq: 0,
        name: "e".into(),
        request: Request {
            method: Method::Get,
            url: "{{x}}".into(),
            headers: vec![],
            params: vec![],
            body: Some(Body {
                kind: BodyKind::Text,
                content: String::new(),
            }),
            auth: None,
        },
    };
    save_endpoint(&root.join("c/e.toml"), &ep).unwrap();
    let step = SequenceStep {
        seq: 0,
        endpoint: "c/e.toml".into(),
        extract: BTreeMap::new(),
        persist: Vec::new(),
    };
    let map = |v: &str| BTreeMap::from([("x".to_owned(), v.to_owned())]);
    let scopes = RunScopes {
        session: map("session"),
        cli: map("cli"),
        profile: map("profile"),
        workspace: map("workspace"),
    };
    // extracted (same-run) still beats session.
    let extracted = map("extracted");
    let prepared = prepare_step(root, &step, &extracted, &scopes).unwrap();
    assert_eq!(prepared.url, "extracted");
    // With no same-run extraction, session wins over cli/profile/workspace.
    let prepared = prepare_step(root, &step, &BTreeMap::new(), &scopes).unwrap();
    assert_eq!(prepared.url, "session");
    // With no session value, cli wins (session sits above cli but is empty).
    let scopes_no_session = RunScopes {
        session: BTreeMap::new(),
        ..scopes.clone()
    };
    let prepared = prepare_step(root, &step, &BTreeMap::new(), &scopes_no_session).unwrap();
    assert_eq!(prepared.url, "cli");
}

#[test]
fn classify_step_extract_error() {
    let mut step_extract = BTreeMap::new();
    step_extract.insert("v".to_owned(), "$.missing".to_owned());
    let step = SequenceStep {
        seq: 0,
        endpoint: "e.toml".into(),
        extract: step_extract,
        persist: Vec::new(),
    };
    let (result, extracted) = classify_step(&Ok(json_response("{}")), &step);
    assert!(matches!(result, StepResult::ExtractError(_)));
    assert!(extracted.is_empty());
}
