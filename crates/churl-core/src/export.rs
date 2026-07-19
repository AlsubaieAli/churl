//! curl command export: generate a paste-safe one-line `curl` invocation from
//! an [`Endpoint`].
//!
//! Round-trip contract with [`crate::import`]: `import_curl(export_curl(e))`
//! reproduces the same method, URL, headers, and body. Enabled params are
//! appended to the URL query (import keeps the query in the URL string), and
//! only enabled headers are emitted. Every argument is shell-quoted via
//! [`shlex::try_quote`]; single spaces, no line continuations.
//!
//! Auth round-trip addendum: [`Auth::Basic`] exports as `-u 'user:pass'`
//! and [`Auth::Bearer`] as `-H 'Authorization: Bearer <token>'` — both
//! round-trip *structurally* (re-import reproduces `request.auth`) as long as
//! secret values are `{{...}}` placeholders, which workspace files guarantee.
//! [`Auth::ApiKey`] exports to its wire form — a `-H 'name: value'` header or a
//! URL query pair — and re-imports as a plain header / URL query, i.e.
//! *wire-equivalent*, not structural. Values are emitted verbatim, placeholders
//! included.

use crate::auth::{AuthWire, apply_auth};
use crate::model::{Auth, Endpoint, Method, Request};

/// Renders `endpoint` as a one-line `curl` command.
///
/// `-X` is omitted for a body-less GET (curl's default); a GET *with* a body
/// emits `-X GET` so the round-trip method survives import's body-implies-POST
/// inference.
pub fn export_curl(endpoint: &Endpoint) -> String {
    let request = &endpoint.request;
    let mut args: Vec<String> = vec!["curl".to_owned()];
    if !(request.method == Method::Get && request.body.is_none()) {
        args.push("-X".to_owned());
        args.push(request.method.to_string());
    }
    if request.insecure {
        // Round-trips the durable per-endpoint insecure-TLS opt-in (import bakes
        // `-k` onto the endpoint; export re-emits it).
        args.push("-k".to_owned());
    }
    match &request.auth {
        Some(Auth::Basic { username, password }) => {
            args.push("-u".to_owned());
            args.push(format!("{username}:{password}"));
        }
        // Bearer and header-placed api keys export as their wire header;
        // query-placed api keys are appended to the URL in `url_with_params`.
        Some(auth) => {
            if let AuthWire::Header { name, value } = apply_auth(auth) {
                args.push("-H".to_owned());
                args.push(format!("{name}: {value}"));
            }
        }
        None => {}
    }
    for header in request.headers.iter().filter(|header| header.enabled) {
        args.push("-H".to_owned());
        args.push(format!("{}: {}", header.name, header.value));
    }
    if let Some(body) = &request.body {
        args.push("--data".to_owned());
        args.push(body.content.clone());
    }
    args.push(url_with_params(request));

    args.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The request URL with every enabled param appended to its query string
/// (percent-encoded), preserving any query already present. A query-placed
/// api-key auth is appended last, mirroring execute's injection order.
fn url_with_params(request: &Request) -> String {
    let mut url = request.url.clone();
    let push_pair = |url: &mut String, name: &str, value: &str| {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&percent_encode(name));
        url.push('=');
        url.push_str(&percent_encode(value));
    };
    for param in request.params.iter().filter(|param| param.enabled) {
        push_pair(&mut url, &param.name, &param.value);
    }
    if let Some(auth) = &request.auth
        && let AuthWire::Query { name, value } = apply_auth(auth)
    {
        push_pair(&mut url, &name, &value);
    }
    url
}

/// Percent-encodes everything outside the RFC 3986 unreserved set.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Shell-quotes one argument. `shlex::try_quote` only fails on interior NUL
/// bytes — those are stripped (they cannot be passed through a shell at all).
fn quote(arg: &str) -> String {
    match shlex::try_quote(arg) {
        Ok(quoted) => quoted.into_owned(),
        Err(_) => {
            let cleaned: String = arg.chars().filter(|&c| c != '\0').collect();
            shlex::try_quote(&cleaned)
                .map(|quoted| quoted.into_owned())
                .unwrap_or(cleaned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiKeyPlacement, Auth, Body, BodyKind, Header, Param};

    fn endpoint(request: Request) -> Endpoint {
        Endpoint {
            seq: 0,
            name: "test".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request,
        }
    }

    fn get(url: &str) -> Request {
        Request {
            method: Method::Get,
            url: url.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        }
    }

    #[test]
    fn bare_get_omits_method_flag() {
        let cmd = export_curl(&endpoint(get("https://example.com/health")));
        assert_eq!(cmd, "curl https://example.com/health");
    }

    #[test]
    fn insecure_endpoint_emits_dash_k_and_round_trips() {
        let mut request = get("https://staging.internal/status");
        request.insecure = true;
        let cmd = export_curl(&endpoint(request));
        assert!(
            cmd.contains(" -k "),
            "insecure endpoint must export -k: {cmd}"
        );
        // A secure endpoint never emits it.
        assert!(!export_curl(&endpoint(get("https://e.com/x"))).contains("-k"));
        // And the flag survives a re-import (structural round-trip).
        let reimported = crate::import::import_curl(&cmd).unwrap();
        assert!(reimported.endpoint.request.insecure);
    }

    #[test]
    fn non_get_method_is_explicit() {
        let mut request = get("https://e.com/x");
        request.method = Method::Delete;
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl -X DELETE https://e.com/x"
        );
    }

    #[test]
    fn get_with_body_keeps_explicit_method() {
        let mut request = get("https://e.com/x");
        request.body = Some(Body {
            kind: BodyKind::Text,
            content: "ping".to_owned(),
        });
        let cmd = export_curl(&endpoint(request));
        assert!(cmd.starts_with("curl -X GET "), "{cmd}");
    }

    #[test]
    fn headers_and_body_are_quoted() {
        let mut request = get("https://e.com/x");
        request.method = Method::Post;
        request.headers = vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }];
        request.body = Some(Body {
            kind: BodyKind::Json,
            content: r#"{"name": "Ada Lovelace"}"#.to_owned(),
        });
        let cmd = export_curl(&endpoint(request));
        assert_eq!(
            cmd,
            r#"curl -X POST -H 'Content-Type: application/json' --data '{"name": "Ada Lovelace"}' https://e.com/x"#
        );
    }

    #[test]
    fn disabled_headers_and_params_are_excluded() {
        let mut request = get("https://e.com/x");
        request.headers = vec![Header {
            name: "X-Debug".to_owned(),
            value: "1".to_owned(),
            enabled: false,
        }];
        request.params = vec![Param {
            name: "trace".to_owned(),
            value: "on".to_owned(),
            enabled: false,
        }];
        assert_eq!(export_curl(&endpoint(request)), "curl https://e.com/x");
    }

    #[test]
    fn basic_auth_exports_as_dash_u() {
        let mut request = get("https://e.com/x");
        request.auth = Some(Auth::Basic {
            username: "alice".to_owned(),
            password: "{{password}}".to_owned(),
        });
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl -u 'alice:{{password}}' https://e.com/x"
        );
    }

    #[test]
    fn bearer_auth_exports_as_authorization_header() {
        let mut request = get("https://e.com/x");
        request.auth = Some(Auth::Bearer {
            token: "{{token}}".to_owned(),
        });
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl -H 'Authorization: Bearer {{token}}' https://e.com/x"
        );
    }

    #[test]
    fn apikey_header_auth_exports_as_header() {
        let mut request = get("https://e.com/x");
        request.auth = Some(Auth::ApiKey {
            name: "X-Api-Key".to_owned(),
            value: "{{api_key}}".to_owned(),
            placement: ApiKeyPlacement::Header,
        });
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl -H 'X-Api-Key: {{api_key}}' https://e.com/x"
        );
    }

    #[test]
    fn apikey_query_auth_appends_to_url_after_params() {
        let mut request = get("https://e.com/search?q=rust");
        request.params = vec![Param {
            name: "page".to_owned(),
            value: "2".to_owned(),
            enabled: true,
        }];
        request.auth = Some(Auth::ApiKey {
            name: "api key".to_owned(),
            value: "{{api_key}}".to_owned(),
            placement: ApiKeyPlacement::Query,
        });
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl 'https://e.com/search?q=rust&page=2&api%20key=%7B%7Bapi_key%7D%7D'"
        );
    }

    #[test]
    fn enabled_params_append_to_query_encoded() {
        let mut request = get("https://e.com/search?q=rust");
        request.params = vec![
            Param {
                name: "page size".to_owned(),
                value: "2&3".to_owned(),
                enabled: true,
            },
            Param {
                name: "ok".to_owned(),
                value: "yes".to_owned(),
                enabled: true,
            },
        ];
        assert_eq!(
            export_curl(&endpoint(request)),
            "curl 'https://e.com/search?q=rust&page%20size=2%263&ok=yes'"
        );
    }
}
