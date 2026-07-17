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
use crate::model::{Endpoint, Header, Method, Param, Request};
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
    /// Scope note: the request **body**, if present, is exported verbatim —
    /// `churl-core` has no body-content masking primitive (only header and
    /// URL span masking exist). A body carrying a resolved `{{secret}}` is
    /// not masked here; this mirrors `resolved_display`'s `body_present`-only
    /// policy but is a real gap for a body-embedded secret and is flagged as
    /// follow-up, not an oversight.
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

        let endpoint = Endpoint {
            seq: 0,
            name: String::new(),
            request: masked,
            assertions: Vec::new(),
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
    use crate::model::{Auth, Body, BodyKind, Header as ModelHeader};

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
        request.body = Some(Body {
            kind: BodyKind::Json,
            content: r#"{"secret":"sk-live-abcdefghijklmnopqrstuvwx"}"#.to_owned(),
        });
        let trace = DebugTrace::from_request(&request);
        assert!(trace.resolved_display.body_present);
    }
}
