//! curl import → export → import round-trip corpus: ≥ 20 realistic commands.
//!
//! The contract (pinned in the M4 design): re-importing an exported command
//! reproduces the same method, URL, headers, and body (semantic equality on the
//! whole `Request`; params round-trip via the URL query since import keeps the
//! query in the URL).

use churl_core::export::export_curl;
use churl_core::import::import_curl;
use churl_core::model::{BodyKind, Method};

/// Realistic curl commands (GitHub/Stripe/Slack-style APIs) with mixed flags
/// and quoting edge cases.
const CORPUS: &[&str] = &[
    // Bare GETs, positional and --url.
    "curl https://api.github.com/repos/rust-lang/rust",
    "curl --url https://api.example.com/v1/status",
    // Query strings (stay in the URL) and quoting.
    "curl 'https://api.github.com/search/repositories?q=tui+language:rust&sort=stars'",
    "curl 'https://api.example.com/search?q=hello world&lang=en'",
    // Headers: bearer auth, custom, values with colons, single/double quotes.
    "curl -H 'Accept: application/vnd.github+json' -H 'Authorization: Bearer ghp_16C7e42F292c69' https://api.github.com/user/repos",
    "curl -H \"X-Api-Key: abc123\" -H \"X-Request-Id: 7f3d\" https://api.example.com/v2/things",
    "curl -H 'X-Forwarded-For: 10.0.0.1:8080' https://api.example.com/whoami",
    "curl -H 'User-Agent: churl test agent (v0.1)' https://api.example.com/agent",
    // JSON bodies: spaces, nested quotes, arrays, unicode.
    r##"curl -X POST https://slack.com/api/chat.postMessage -H 'Content-Type: application/json' -H 'Authorization: Bearer xoxb-123' --data '{"channel": "#general", "text": "hello world"}'"##,
    r#"curl -d "{\"name\": \"Ada Lovelace\", \"tags\": [\"math\", \"pioneer\"]}" https://api.example.com/users"#,
    r#"curl --json '{"query": "mutation { addItem(name: \"x\") { id } }"}' https://api.example.com/graphql"#,
    r#"curl --json '{"items": [1, 2, 3]}' -H 'Accept: application/json' https://api.example.com/bulk"#,
    // Form bodies: multiple -d joined with &, --data-raw with &.
    "curl https://api.stripe.com/v1/charges -u sk_test_4eC39HqLyjWDarjtT1zdp7dc: -d amount=2000 -d currency=usd -d 'description=My First Test Charge'",
    "curl --data-raw 'grant_type=client_credentials&scope=read write&client_id=abc' https://auth.example.com/oauth/token",
    "curl -d a=1 -d b=2 -d c=3 https://httpbin.org/post",
    // Explicit methods, long/short/=/attached spellings.
    "curl -X DELETE https://api.example.com/items/42 -H 'X-Api-Key: secret123'",
    "curl --request PUT --url https://api.example.com/items/9 --header 'Content-Type: application/json' --data '{\"done\": true}'",
    "curl --request=PATCH --data='{\"active\": false}' https://api.example.com/users/1",
    "curl -XPOST https://api.example.com/quick -d hello",
    "curl -X OPTIONS https://api.example.com/cors -H 'Origin: https://app.example.com'",
    "curl -X HEAD https://api.example.com/ping",
    // Clustered value-less shorts, ignored flags, basic auth.
    "curl -sSL https://example.com/health",
    "curl -sS --compressed -k https://staging.internal/api/v2/status",
    "curl -o export.json -u alice:s3cr3t https://api.example.com/private/export",
    // Text body that is neither JSON nor form.
    "curl -d 'plain text payload, no structure' -H 'Content-Type: text/plain' https://api.example.com/echo",
];

#[test]
fn corpus_round_trips_semantically() {
    assert!(CORPUS.len() >= 20, "corpus must stay at ≥ 20 commands");
    for command in CORPUS {
        let first = import_curl(command)
            .unwrap_or_else(|err| panic!("first import failed for {command:?}: {err}"));
        let exported = export_curl(&first.endpoint);
        let second = import_curl(&exported).unwrap_or_else(|err| {
            panic!("re-import failed for {command:?}\nexported: {exported}\nerror: {err}")
        });
        assert_eq!(
            first.endpoint.request, second.endpoint.request,
            "request round-trip mismatch\ncommand:  {command}\nexported: {exported}"
        );
        assert_eq!(
            first.endpoint.name, second.endpoint.name,
            "name round-trip mismatch for {command:?} (exported: {exported})"
        );
    }
}

#[test]
fn stripe_style_form_post_imports_faithfully() {
    let result = import_curl(
        "curl https://api.stripe.com/v1/charges -u sk_test_abc: -d amount=2000 -d currency=usd",
    )
    .unwrap();
    let request = &result.endpoint.request;
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.url, "https://api.stripe.com/v1/charges");
    let body = request.body.as_ref().unwrap();
    assert_eq!(body.content, "amount=2000&currency=usd");
    assert_eq!(body.kind, BodyKind::Form);
    assert!(
        request
            .headers
            .iter()
            .any(|h| h.name == "Authorization" && h.value.starts_with("Basic "))
    );
    assert_eq!(result.endpoint.name, "charges");
}

#[test]
fn exported_command_is_a_single_paste_safe_line() {
    let result = import_curl(
        r#"curl --json '{"text": "multi word value"}' 'https://api.example.com/send?dry run=1'"#,
    )
    .unwrap();
    let exported = export_curl(&result.endpoint);
    assert!(
        !exported.contains('\n'),
        "no line continuations: {exported}"
    );
    // shlex must tokenise it back without loss.
    assert!(shlex_ok(&exported), "not shell-safe: {exported}");
}

/// Returns whether shlex can re-tokenise the command.
fn shlex_ok(command: &str) -> bool {
    import_curl(command).is_ok()
}
