//! Per-exchange debug trace capture.
//!
//! [`DebugTrace`] is the in-memory record of what churl actually did while
//! resolving and sending one request: the masked resolved-request projection,
//! the `{{var}}` resolution steps, every redirect hop followed, the
//! auth/cookie/proxy decisions made, and (on failure) the mapped error's
//! cause chain. It backs the Inspector overlay (TUI) and the machine-readable
//! `-v` trace (headless `--json`) added in later waves.
//!
//! Capture is opt-in and additive: every call site that fills a `DebugTrace`
//! is gated behind `Option<&mut DebugTrace>` being `Some` (see
//! [`crate::http::execute_traced`], [`crate::template::Resolver::substitute_request_traced`]).
//! A `None` sink builds and pushes nothing — the zero-overhead path a normal
//! send with debugging off always takes.
//!
//! Secrets never round-trip through a trace: `resolved_display` mirrors the
//! headless JSON-envelope masking discipline
//! ([`crate::secrets::mask_header_value`] / [`crate::secrets::mask_url`]),
//! `resolved_raw` is in-memory only (`#[serde(skip)]`, never serialized,
//! rendered, or copied), and [`DebugTrace::masked_curl`] renders from a masked
//! clone, never the raw resolved request.

use serde::Serialize;

use crate::auth::{AuthWire, apply_auth};
use crate::export;
use crate::http::HttpError;
use crate::model::{Body, Endpoint, Header, Method, Param, PartValue, Request};
use crate::secrets;

/// A masked, display-safe projection of a resolved request. Never carries a
/// raw secret: `url` is masked via [`secrets::mask_url`], each header value
/// via [`secrets::mask_header_value`]. The body is never echoed — only
/// whether one is present — mirroring the existing headless JSON-envelope
/// request projection, which has no body-masking primitive to reuse safely.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRequest {
    /// The request method.
    pub method: Method,
    /// The request URL with every secret span masked (see [`secrets::mask_url`]).
    pub url: String,
    /// Enabled request headers, each value masked via [`secrets::mask_header_value`].
    /// Omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// Whether the request carries a body. The body content itself is never
    /// captured — matching the M8.2 headless request-echo policy.
    pub body_present: bool,
}

impl ResolvedRequest {
    /// Projects `request` into its masked display form.
    fn from_request(request: &Request) -> Self {
        Self {
            method: request.method,
            url: secrets::mask_url(&request.url),
            headers: request
                .headers
                .iter()
                .filter(|header| header.enabled)
                .map(|header| Header {
                    name: header.name.clone(),
                    value: secrets::mask_header_value(&header.name, &header.value),
                    enabled: true,
                })
                .collect(),
            body_present: request.body.is_some(),
        }
    }
}

/// One `{{var}}` placeholder resolution recorded during template
/// substitution (see [`crate::template::Resolver::substitute_request_traced`]).
#[derive(Debug, Clone, Serialize)]
pub struct VarStep {
    /// The placeholder name (without the surrounding `{{`/`}}`).
    pub name: String,
    /// The scope that supplied the value (`"session"`, `"cli"`, `"profile"`,
    /// `"collection"`, …). `None` means the process-environment fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<&'static str>,
    /// The resolved value, masked via [`secrets::looks_like_secret_value`]
    /// when it looks secret-shaped. Display-only — never fed back into the
    /// actual substitution, which is byte-identical to the untraced path.
    pub value_masked: String,
}

/// One redirect hop followed while executing a request (see
/// [`crate::http::execute_traced`]).
#[derive(Debug, Clone, Serialize)]
pub struct RedirectHop {
    /// The hop's source URL, masked via [`secrets::mask_url`].
    pub from: String,
    /// The hop's target URL (the `Location` the response resolved to), masked
    /// via [`secrets::mask_url`].
    pub to: String,
    /// The redirect's HTTP status code.
    pub status: u16,
    /// The method change this hop applied, if any (e.g. a 303 downgrading
    /// `POST` to `GET`). `None` when the method was preserved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method_change: Option<(Method, Method)>,
    /// Whether this hop crosses the origin (scheme + host + port) of the
    /// previous request.
    pub cross_origin: bool,
    /// Names of the headers stripped before following this hop (populated
    /// only under [`crate::config::RedirectPolicy::Strip`] on a cross-origin
    /// hop; see [`crate::http`]'s `strip_auth_headers`). Omitted from
    /// serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_headers: Vec<String>,
}

/// Auth/cookie/proxy decisions made while shaping the outgoing request.
///
/// `cookie_used`/`proxy` stay at their defaults (`false`/`None`) from
/// [`crate::http::execute_traced`] alone: the send path only has access to an
/// already-built `reqwest::Client` (see [`crate::http::execute`]'s module
/// docs), not the [`crate::http::ClientConfig`] that carries the proxy URL or
/// cookie-jar handle. A caller that holds the `ClientConfig` (the TUI/headless
/// session layer, in a later wave) may set these itself after
/// `execute_traced` returns.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuthCookieProxyDecisions {
    /// The name of the auth-bearing header or query parameter churl injected
    /// for this request, if any (`None` when the request has no first-class
    /// auth, or when an enabled user header of the same name already won).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_injected: Option<String>,
    /// Whether a cookie jar was used for this request. See the field-group
    /// docs above — unset (stays `false`) by `execute_traced` itself.
    pub cookie_used: bool,
    /// The proxy URL used for this request, masked, if any. See the
    /// field-group docs above — unset (stays `None`) by `execute_traced`
    /// itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// The mapped [`HttpError`] plus its `std::error::Error` source chain,
/// captured on a failed send.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorCause {
    /// The error's `Display` message (matches what the CLI/TUI already show).
    pub message: String,
    /// Each `Error::source()` hop, outermost cause first. Empty when the
    /// error has no further source (e.g. [`HttpError::Timeout`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_chain: Vec<String>,
}

/// Per-exchange debug capture sink. See the module docs for the zero-overhead
/// and never-leak-secrets discipline every capture site follows.
#[derive(Debug, Clone, Serialize)]
pub struct DebugTrace {
    /// The masked, display-safe resolved request. Refreshed by
    /// [`crate::http::execute_traced`] from the exact request it sends, so
    /// this always reflects the final (post `{{var}}` substitution) request
    /// even when the trace was created earlier (e.g. before substitution).
    pub resolved_display: ResolvedRequest,
    /// The raw resolved request, in-memory only. **Never** serialized
    /// (`#[serde(skip)]`), rendered, or copied — [`DebugTrace::masked_curl`]
    /// is the only sanctioned way to turn this into text, and it masks first.
    #[serde(skip)]
    pub resolved_raw: Request,
    /// Every `{{var}}` placeholder resolved while substituting this request,
    /// in substitution order. Omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub var_steps: Vec<VarStep>,
    /// Every redirect hop followed, in hop order. Omitted from serialized
    /// output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_hops: Vec<RedirectHop>,
    /// Auth/cookie/proxy decisions made for this request.
    pub decisions: AuthCookieProxyDecisions,
    /// The cause of a failed send, if this exchange failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCause>,
}

impl DebugTrace {
    /// Starts a trace from `request`: seeds the masked display projection and
    /// stashes the raw request in-memory. Everything else starts empty and is
    /// filled in as var-resolution and [`crate::http::execute_traced`]
    /// progress. Callable at any stage (before or after `{{var}}`
    /// substitution) — `execute_traced` re-derives `resolved_display`/
    /// `resolved_raw` from the actual request it sends as its first capture
    /// step, so a trace started pre-substitution still ends up reflecting the
    /// final wire request.
    pub fn from_request(request: &Request) -> Self {
        Self {
            resolved_display: ResolvedRequest::from_request(request),
            resolved_raw: request.clone(),
            var_steps: Vec::new(),
            redirect_hops: Vec::new(),
            decisions: AuthCookieProxyDecisions::default(),
            error: None,
        }
    }

    /// Refreshes `resolved_display`/`resolved_raw` from `request` — the exact
    /// request about to be sent. See [`DebugTrace::from_request`]'s docs for
    /// why this is called again inside `execute_traced` even when the sink
    /// was created earlier.
    pub(crate) fn capture_request(&mut self, request: &Request) {
        self.resolved_display = ResolvedRequest::from_request(request);
        self.resolved_raw = request.clone();
    }

    /// Renders the resolved request as a masked `curl` command: header values
    /// and the URL are masked exactly like `resolved_display` before export,
    /// and any first-class auth ([`crate::model::Auth`]) is folded into a
    /// masked header/query pair rather than being handed to
    /// [`export::export_curl`] raw — that function has no masked-auth path of
    /// its own (`Auth::Basic` renders as `-u user:pass`, everything else via
    /// [`apply_auth`], both unmasked). The rendered command is never the raw
    /// resolved request (see the module docs, "never leak").
    ///
    /// Scope note: the request **body**, if present, is redacted token-wise by
    /// [`secrets::mask_secret_tokens`] — every secret-*shaped* token
    /// (`sk-live-…`, JWTs, high-entropy runs) is masked while the body's
    /// structure and non-secret values stay readable. The residual gap is a
    /// LOW-ENTROPY body secret that is neither secret-shaped nor caught by the
    /// heuristic (e.g. `{"password":"hunter2"}`) — the body carries no name
    /// anchor to key off, so it is left for the future reveal-gated real-curl
    /// (documented follow-up, not an oversight).
    pub fn masked_curl(&self) -> String {
        let mut masked = self.resolved_raw.clone();

        if let Some(auth) = masked.auth.take() {
            match apply_auth(&auth) {
                AuthWire::Header { name, .. } => masked.headers.push(Header {
                    name,
                    value: secrets::SECRET_MASK.to_owned(),
                    enabled: true,
                }),
                AuthWire::Query { name, .. } => masked.params.push(Param {
                    name,
                    value: secrets::SECRET_MASK.to_owned(),
                    enabled: true,
                }),
            }
        }

        masked.url = secrets::mask_url(&masked.url);
        masked.headers = masked
            .headers
            .into_iter()
            .map(|header| Header {
                value: secrets::mask_header_value(&header.name, &header.value),
                name: header.name,
                enabled: header.enabled,
            })
            .collect();
        // Structured query params are appended verbatim to the query string by
        // `export::url_with_params`, so a resolved secret in one (`api_key =
        // {{secret}}`) would leak raw to the clipboard. Mask by the SAME
        // query-pair rule the URL masking (`secrets::mask_url`) applies to
        // inline `?key=value` pairs — name-anchor ∪ secret-shaped value — so a
        // secret in a param and the same secret in the URL mask identically.
        // The auth-injected param pushed above already carries `SECRET_MASK`,
        // so it is inert here.
        for param in &mut masked.params {
            if crate::config::looks_like_secret_name(&param.name)
                || secrets::looks_like_secret_value(&param.value)
            {
                param.value = secrets::SECRET_MASK.to_owned();
            }
        }
        if let Some(body) = masked.body.as_mut() {
            match body {
                Body::Simple { content, .. } => {
                    *content = secrets::mask_secret_tokens(content);
                }
                // File CONTENTS are never touched (never read here at all), but
                // the part's path / filename / mime STRINGS can carry a resolved
                // `{{secret}}` template (`-F name=@{{token}}` → `export_curl`
                // emits `-F name=@<secret>`), so token-mask each of them — the
                // same leak class as the M8.3 query-param P0. Inline text parts
                // are masked exactly like a `Simple` body.
                Body::Multipart(parts) => {
                    for part in parts.iter_mut() {
                        match &mut part.value {
                            PartValue::Text(text) => {
                                *text = secrets::mask_secret_tokens(text);
                            }
                            PartValue::File {
                                path,
                                filename,
                                mime,
                            } => {
                                *path = secrets::mask_secret_tokens(path);
                                if let Some(filename) = filename {
                                    *filename = secrets::mask_secret_tokens(filename);
                                }
                                if let Some(mime) = mime {
                                    *mime = secrets::mask_secret_tokens(mime);
                                }
                            }
                        }
                    }
                }
            }
        }

        let endpoint = Endpoint {
            seq: 0,
            name: String::new(),
            request: masked,
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
        };
        export::export_curl(&endpoint)
    }

    /// Records a failed send: the mapped error's display message plus its
    /// `std::error::Error` source chain (see [`ErrorCause`]).
    pub fn record_error(&mut self, err: &HttpError) {
        let mut source_chain = Vec::new();
        let mut source = std::error::Error::source(err);
        while let Some(next) = source {
            source_chain.push(next.to_string());
            source = next.source();
        }
        self.error = Some(ErrorCause {
            message: err.to_string(),
            source_chain,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Body, BodyKind, Header as ModelHeader, Part, PartValue};

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
    fn from_request_masks_display_and_keeps_raw() {
        let mut request = get("https://example.com/x?api_key=sk-live-abcdefghijklmnopqrstuvwx");
        request.headers.push(ModelHeader {
            name: "Authorization".to_owned(),
            value: "Bearer supersecret123456789012345".to_owned(),
            enabled: true,
        });
        let trace = DebugTrace::from_request(&request);

        assert!(!trace.resolved_display.url.contains("sk-live-"));
        assert!(
            trace
                .resolved_display
                .headers
                .iter()
                .all(|h| h.value != "Bearer supersecret123456789012345")
        );
        // The raw copy is untouched — masking never mutates it.
        assert_eq!(trace.resolved_raw.url, request.url);
        assert_eq!(
            trace.resolved_raw.headers[0].value,
            "Bearer supersecret123456789012345"
        );
    }

    #[test]
    fn masked_curl_never_leaks_bearer_auth() {
        let mut request = get("https://example.com/x");
        request.auth = Some(Auth::Bearer {
            token: "supersecret123456789012345".to_owned(),
        });
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();

        assert!(!curl.contains("supersecret123456789012345"));
        assert!(curl.contains("Authorization"));
    }

    #[test]
    fn masked_curl_never_leaks_secret_query_params() {
        let mut request = get("https://example.com/x");
        // A secret-SHAPED value under a non-secret name.
        request.params.push(Param {
            name: "api_key".to_owned(),
            value: "sk-live-abcdefghijklmnopqrstuvwx".to_owned(),
            enabled: true,
        });
        // A low-entropy secret under a secret-NAMED param (only the name
        // anchor catches this — the value alone would not).
        request.params.push(Param {
            name: "token".to_owned(),
            value: "hunter2".to_owned(),
            enabled: true,
        });
        // A plain non-secret param must survive verbatim.
        request.params.push(Param {
            name: "page".to_owned(),
            value: "1".to_owned(),
            enabled: true,
        });
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();

        assert!(
            !curl.contains("sk-live-abcdefghijklmnopqrstuvwx"),
            "secret-shaped query param leaked: {curl}"
        );
        assert!(
            !curl.contains("hunter2"),
            "secret-named query param leaked: {curl}"
        );
        assert!(
            curl.contains("page") && curl.contains('1'),
            "non-secret param must be preserved: {curl}"
        );
    }

    #[test]
    fn masked_curl_never_leaks_secret_named_or_shaped_headers() {
        let mut request = get("https://example.com/x");
        request.headers.push(ModelHeader {
            name: "X-Api-Token".to_owned(),
            value: "abc".to_owned(),
            enabled: true,
        });
        request.headers.push(ModelHeader {
            name: "X-Weird".to_owned(),
            value: "sk-live-abcdefghijklmnopqrstuvwxyz012345".to_owned(),
            enabled: true,
        });
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();

        assert!(!curl.contains("sk-live-abcdefghijklmnopqrstuvwxyz012345"));
        // "abc" is too short/low-entropy to leak-check directly, but the
        // secret-named header must still be masked in the display projection.
        assert!(
            trace
                .resolved_display
                .headers
                .iter()
                .find(|h| h.name == "X-Api-Token")
                .is_some_and(|h| h.value == secrets::SECRET_MASK)
        );
    }

    #[test]
    fn record_error_captures_message() {
        let mut trace = DebugTrace::from_request(&get("https://example.com"));
        trace.record_error(&HttpError::Timeout);
        let error = trace.error.expect("error recorded");
        assert_eq!(error.message, "request timed out");
        assert!(error.source_chain.is_empty());
    }

    #[test]
    fn body_present_reflects_body_without_echoing_content() {
        let mut request = get("https://example.com");
        request.body = Some(Body::Simple {
            kind: BodyKind::Json,
            content: r#"{"secret":"sk-live-abcdefghijklmnopqrstuvwx"}"#.to_owned(),
        });
        let trace = DebugTrace::from_request(&request);
        assert!(trace.resolved_display.body_present);
    }

    #[test]
    fn masked_curl_masks_secret_shaped_body_token() {
        let mut request = get("https://example.com/x");
        request.method = Method::Post;
        request.body = Some(Body::Simple {
            kind: BodyKind::Json,
            content: r#"{"token":"sk-live-abcdefghijklmnopqrstuvwx"}"#.to_owned(),
        });
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();
        assert!(
            !curl.contains("sk-live-abcdefghijklmnopqrstuvwx"),
            "body secret leaked into masked_curl: {curl}"
        );
        assert!(curl.contains(secrets::SECRET_MASK));
    }

    #[test]
    fn masked_curl_preserves_structural_body() {
        let mut request = get("https://example.com/x");
        request.method = Method::Post;
        request.body = Some(Body::Simple {
            kind: BodyKind::Json,
            content: r#"{"page":1,"q":"orders"}"#.to_owned(),
        });
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();
        // No secret-shaped token → the body survives verbatim inside the curl.
        assert!(
            curl.contains(r#"{"page":1,"q":"orders"}"#),
            "structural body over-masked: {curl}"
        );
    }

    /// F1 (security): a resolved `{{secret}}` in a multipart FILE part's
    /// `path`/`filename`/`mime` must be masked in `masked_curl` (which
    /// `export_curl`s the parts as `-F name=@path;filename=…;type=…`) — the
    /// same leak class as the M8.3 query-param P0. File CONTENTS are never
    /// read here, but these path/filename/mime STRINGS can carry a templated
    /// credential.
    #[test]
    fn masked_curl_masks_secret_in_file_part_path_filename_and_mime() {
        const SECRET: &str = "sk-live-abcdefghijklmnopqrstuvwx";
        let mut request = get("https://example.com/upload");
        request.method = Method::Post;
        request.body = Some(Body::Multipart(vec![Part {
            name: "upload".to_owned(),
            value: PartValue::File {
                // Each of these stands in for a resolved `{{secret}}` template.
                path: SECRET.to_owned(),
                filename: Some(SECRET.to_owned()),
                mime: Some(SECRET.to_owned()),
            },
        }]));
        let trace = DebugTrace::from_request(&request);
        let curl = trace.masked_curl();
        assert!(
            !curl.contains(SECRET),
            "a secret in a file part path/filename/mime leaked into masked_curl: {curl}"
        );
        assert!(curl.contains(secrets::SECRET_MASK), "{curl}");
    }

    /// A multipart TEXT part's secret value stays masked too (unchanged from
    /// before F1 — asserts the Text arm didn't regress alongside the File fix).
    #[test]
    fn masked_curl_masks_secret_in_text_part() {
        const SECRET: &str = "sk-live-abcdefghijklmnopqrstuvwx";
        let mut request = get("https://example.com/upload");
        request.method = Method::Post;
        request.body = Some(Body::Multipart(vec![Part {
            name: "token".to_owned(),
            value: PartValue::Text(SECRET.to_owned()),
        }]));
        let curl = DebugTrace::from_request(&request).masked_curl();
        assert!(!curl.contains(SECRET), "{curl}");
        assert!(curl.contains(secrets::SECRET_MASK), "{curl}");
    }
}
