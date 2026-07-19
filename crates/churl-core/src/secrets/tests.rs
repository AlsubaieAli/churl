use super::*;
use crate::model::{ApiKeyPlacement, Body, BodyKind, Header, Method, Param, Profile, Request};
use std::collections::BTreeMap;

fn endpoint(auth: Option<Auth>) -> Endpoint {
    Endpoint {
        seq: 0,
        name: "e".to_owned(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Get,
            url: "https://example.com".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth,
            insecure: false,
        },
    }
}

fn locations(findings: &[SecretFinding]) -> Vec<&str> {
    findings.iter().map(|f| f.location.as_str()).collect()
}

// --- Value-shape heuristic ---

#[test]
fn secret_shaped_values_detected() {
    assert!(looks_like_secret_value("sk-abc123DEF456ghi789JK")); // Stripe-style
    assert!(looks_like_secret_value(
        "ghp_0123456789abcdefABCDEF0123456789abcd"
    ));
    assert!(looks_like_secret_value("xoxb-1234567890-abcdefghij"));
    assert!(looks_like_secret_value("AKIAIOSFODNN7EXAMPLE"));
    // A JWT.
    assert!(looks_like_secret_value(
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.SflKxwRJSMeKKF2QT4fwpMeJf36"
    ));
    // A long high-entropy mixed run.
    assert!(looks_like_secret_value(
        "aB3dE7fG9hJ2kL4mN6pQ8rS0tU1vW3xY5zA7bC9dE1f"
    ));
}

#[test]
fn innocent_values_not_shaped() {
    assert!(!looks_like_secret_value(""));
    assert!(!looks_like_secret_value("hello"));
    assert!(!looks_like_secret_value("application/json"));
    assert!(!looks_like_secret_value("https://api.example.com/v1/users"));
    // A `{{placeholder}}` is never secret-shaped.
    assert!(!looks_like_secret_value("{{token}}"));
    // A long but all-lowercase, no-digit word is not flagged (no entropy signal).
    assert!(!looks_like_secret_value(
        "thisisaverylonglowercaseidentifierwithoutdigits"
    ));
    // Short vendor-looking but too short.
    assert!(!looks_like_secret_value("sk-abc"));
}

// --- Endpoint scan: A3 coverage ---

#[test]
fn auth_findings_are_name_anchored_block() {
    let f = scan_endpoint(&endpoint(Some(Auth::Bearer {
        token: "literal-token".to_owned(),
    })));
    assert_eq!(locations(&f), vec!["auth.token"]);
    assert_eq!(f[0].severity, Severity::Block);

    // A `{{placeholder}}` token is clean.
    let f = scan_endpoint(&endpoint(Some(Auth::Bearer {
        token: "{{token}}".to_owned(),
    })));
    assert!(f.is_empty());

    // Basic password literal.
    let f = scan_endpoint(&endpoint(Some(Auth::Basic {
        username: "u".to_owned(),
        password: "hunter2".to_owned(),
    })));
    assert_eq!(locations(&f), vec!["auth.password"]);

    // ApiKey with a secret-looking name + literal value.
    let f = scan_endpoint(&endpoint(Some(Auth::ApiKey {
        name: "X-Api-Key".to_owned(),
        value: "literal".to_owned(),
        placement: ApiKeyPlacement::Header,
    })));
    assert_eq!(locations(&f), vec!["auth.value"]);
}

#[test]
fn secret_header_value_blocks_when_named() {
    let mut ep = endpoint(None);
    ep.request.headers.push(Header {
        name: "Authorization".to_owned(),
        value: "Bearer sk-livexyz".to_owned(),
        enabled: true,
    });
    let f = scan_endpoint(&ep);
    assert_eq!(locations(&f), vec!["headers.Authorization"]);
    assert_eq!(f[0].severity, Severity::Block);

    // A placeholder value clears it.
    ep.request.headers[0].value = "{{auth}}".to_owned();
    assert!(scan_endpoint(&ep).is_empty());
}

#[test]
fn innocent_header_with_shaped_value_warns() {
    let mut ep = endpoint(None);
    ep.request.headers.push(Header {
        name: "X-Trace".to_owned(),
        value: "ghp_0123456789abcdefABCDEF0123456789abcd".to_owned(),
        enabled: true,
    });
    let f = scan_endpoint(&ep);
    assert_eq!(locations(&f), vec!["headers.X-Trace"]);
    assert_eq!(f[0].severity, Severity::Warn);
}

#[test]
fn url_query_secret_key_blocks_and_userinfo_blocks() {
    let mut ep = endpoint(None);
    ep.request.url = "https://user:s3cr3tpass@api.example.com/path?api_key=abcd1234".to_owned();
    let f = scan_endpoint(&ep);
    let locs = locations(&f);
    assert!(locs.contains(&"url.userinfo"), "{locs:?}");
    assert!(locs.contains(&"url.query.api_key"), "{locs:?}");
    assert!(f.iter().all(|x| x.severity == Severity::Block));

    // A `{{placeholder}}` password and query value clear both.
    ep.request.url = "https://user:{{pw}}@api.example.com/path?api_key={{k}}".to_owned();
    assert!(scan_endpoint(&ep).is_empty());
}

#[test]
fn param_secret_key_blocks() {
    let mut ep = endpoint(None);
    ep.request.params.push(Param {
        name: "access_token".to_owned(),
        value: "literal".to_owned(),
        enabled: true,
    });
    let f = scan_endpoint(&ep);
    assert_eq!(locations(&f), vec!["params.access_token"]);
    assert_eq!(f[0].severity, Severity::Block);
}

#[test]
fn body_secret_shaped_value_warns_not_blocks() {
    let mut ep = endpoint(None);
    ep.request.body = Some(Body {
        kind: BodyKind::Json,
        content: r#"{"token": "ghp_0123456789abcdefABCDEF0123456789abcd"}"#.to_owned(),
    });
    let f = scan_endpoint(&ep);
    assert_eq!(locations(&f), vec!["body"]);
    assert_eq!(f[0].severity, Severity::Warn);

    // An innocent body produces nothing.
    ep.request.body = Some(Body {
        kind: BodyKind::Json,
        content: r#"{"name": "alice"}"#.to_owned(),
    });
    assert!(scan_endpoint(&ep).is_empty());
}

// --- Workspace / collection scans ---

#[test]
fn workspace_secret_named_literal_blocks() {
    let mut vars = BTreeMap::new();
    vars.insert("api_key".to_owned(), "literal".to_owned());
    vars.insert("base_url".to_owned(), "https://x".to_owned());
    let ws = Workspace {
        name: "w".to_owned(),
        vars,
        profiles: vec![Profile {
            name: "prod".to_owned(),
            vars: {
                let mut m = BTreeMap::new();
                m.insert("token".to_owned(), "abc".to_owned());
                m
            },
        }],
        ..Default::default()
    };
    let f = scan_workspace(&ws);
    let locs = locations(&f);
    assert!(locs.contains(&"vars.api_key"), "{locs:?}");
    assert!(locs.contains(&"prod.token"), "{locs:?}");
    assert!(f.iter().all(|x| x.severity == Severity::Block));
}

#[test]
fn workspace_innocent_shaped_value_warns() {
    let mut vars = BTreeMap::new();
    vars.insert(
        "trace".to_owned(),
        "ghp_0123456789abcdefABCDEF0123456789abcd".to_owned(),
    );
    let ws = Workspace {
        name: "w".to_owned(),
        vars,
        profiles: Vec::new(),
        ..Default::default()
    };
    let f = scan_workspace(&ws);
    assert_eq!(locations(&f), vec!["vars.trace"]);
    assert_eq!(f[0].severity, Severity::Warn);
}

// --- The decision engine: novelty × severity × policy ---

fn block(loc: &str) -> SecretFinding {
    SecretFinding::block(loc)
}
fn warn(loc: &str) -> SecretFinding {
    SecretFinding::warn(loc)
}

#[test]
fn new_name_anchored_blocks_under_strict() {
    let d = decide(&[block("vars.api_key")], &[], SecretPolicy::Strict);
    assert!(d.is_refused());
    assert_eq!(d.refusal_locations(), vec!["vars.api_key".to_owned()]);
    assert!(d.warnings.is_empty());
}

#[test]
fn pre_existing_name_anchored_grandfathers_to_warning() {
    // Same location present in baseline → grandfather → warn, not refuse.
    let d = decide(
        &[block("vars.api_key")],
        &[block("vars.api_key")],
        SecretPolicy::Strict,
    );
    assert!(!d.is_refused());
    assert_eq!(d.warning_locations(), vec!["vars.api_key".to_owned()]);
}

#[test]
fn value_only_never_blocks_even_when_new() {
    let d = decide(&[warn("body")], &[], SecretPolicy::Strict);
    assert!(!d.is_refused());
    assert_eq!(d.warning_locations(), vec!["body".to_owned()]);
}

#[test]
fn warn_policy_blocks_nothing() {
    let d = decide(
        &[block("vars.api_key"), warn("body")],
        &[],
        SecretPolicy::Warn,
    );
    assert!(!d.is_refused());
    assert_eq!(
        d.warning_locations(),
        vec!["vars.api_key".to_owned(), "body".to_owned()]
    );
}

#[test]
fn new_file_every_block_finding_is_new() {
    // Empty baseline (brand-new file): a name-anchored literal refuses.
    let d = decide(
        &[block("auth.token"), block("headers.Authorization")],
        &[],
        SecretPolicy::Strict,
    );
    assert_eq!(d.refusals.len(), 2);
}

// --- URL masking (mask_url) ---

#[test]
fn mask_url_redacts_userinfo_password() {
    assert_eq!(
        mask_url("https://admin:s3cr3t@host.example.com/x"),
        format!("https://admin:{SECRET_MASK}@host.example.com/x")
    );
    // Username kept, password gone.
    assert!(!mask_url("https://admin:s3cr3t@host/x").contains("s3cr3t"));
    assert!(mask_url("https://admin:s3cr3t@host/x").contains("admin"));
}

#[test]
fn mask_url_leaves_placeholder_password_untouched() {
    // A `{{var}}` password is not a literal secret — the resolver may not have
    // filled it (send would refuse an unresolved one anyway).
    let url = "https://admin:{{password}}@host/x";
    assert_eq!(mask_url(url), url);
}

#[test]
fn mask_url_redacts_secret_named_query_value() {
    assert_eq!(
        mask_url("https://host/x?api_key=REALKEY&page=2"),
        format!("https://host/x?api_key={SECRET_MASK}&page=2")
    );
    // Innocent pairs survive verbatim.
    assert!(mask_url("https://host/x?api_key=REALKEY&page=2").contains("page=2"));
}

#[test]
fn mask_url_redacts_secret_shaped_query_value_under_innocent_key() {
    // Stripe-style token under a non-secret key name → value-anchor fires.
    let masked = mask_url("https://host/x?ref=sk-abc123DEF456ghi789JKlmno");
    assert!(!masked.contains("sk-abc123DEF456ghi789JKlmno"), "{masked}");
    assert!(masked.contains(&format!("ref={SECRET_MASK}")), "{masked}");
}

#[test]
fn mask_url_preserves_fragment_and_placeholder_query_value() {
    // A templated query value stays; the fragment is untouched.
    let url = "https://host/x?token={{tok}}&q=hi#frag";
    assert_eq!(mask_url(url), url);
}

#[test]
fn mask_url_no_secret_is_byte_identical() {
    let url = "https://host.example.com/path?page=2&sort=asc";
    assert_eq!(mask_url(url), url);
    // No query, no userinfo.
    assert_eq!(mask_url("https://host/plain"), "https://host/plain");
}

#[test]
fn mask_secret_tokens_redacts_secret_shaped_body_value() {
    let body = r#"{"token":"sk-live-abcdefghijklmnopqrstuvwx"}"#;
    let masked = mask_secret_tokens(body);
    assert!(
        !masked.contains("sk-live-abcdefghijklmnopqrstuvwx"),
        "raw secret leaked: {masked}"
    );
    assert!(masked.contains(SECRET_MASK), "mask token missing: {masked}");
    // Structure/keys preserved.
    assert!(masked.starts_with(r#"{"token":"#), "{masked}");
    assert_eq!(masked, format!(r#"{{"token":"{SECRET_MASK}"}}"#));
}

#[test]
fn mask_secret_tokens_preserves_structural_body() {
    // No secret-shaped tokens → byte-identical (no over-masking).
    let body = r#"{"page":1,"q":"orders"}"#;
    assert_eq!(mask_secret_tokens(body), body);
}

#[test]
fn mask_secret_tokens_masks_form_encoded_secret() {
    let body = "grant_type=client_credentials&client_secret=ghp_ABCdef123456GHIjkl789MNOpqr";
    let masked = mask_secret_tokens(body);
    assert!(
        !masked.contains("ghp_ABCdef123456GHIjkl789MNOpqr"),
        "{masked}"
    );
    // Delimiters + non-secret tokens survive.
    assert!(masked.starts_with("grant_type=client_credentials&client_secret="));
    assert!(masked.ends_with(SECRET_MASK));
}

#[test]
fn mask_secret_tokens_low_entropy_body_secret_is_the_documented_gap() {
    // A low-entropy value under a secret-looking KEY is NOT caught — the body
    // has no name anchor. Documented follow-up (reveal-gated real-curl).
    let body = r#"{"password":"hunter2"}"#;
    assert_eq!(mask_secret_tokens(body), body);
}

#[test]
fn mask_secret_tokens_leaves_placeholder_untouched() {
    let body = r#"{"token":"{{secret}}"}"#;
    assert_eq!(mask_secret_tokens(body), body);
}
