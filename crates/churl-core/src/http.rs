//! HTTP request execution via `reqwest` + `rustls`.
//!
//! This module is deliberately runtime-agnostic: [`execute`] is a plain `async fn`
//! with no [`tokio`](https://docs.rs/tokio) types in its signature. Cancellation is
//! handled at the *task* level by the caller — the TUI spawns the future with
//! `tokio::spawn` and keeps the resulting `AbortHandle` — so `churl-core` never needs
//! to know about the runtime. There is no `{{var}}` templating here; URLs, headers,
//! and body are used verbatim.
//!
//! First-class auth is injected here: the request's [`crate::model::Auth`]
//! resolves to an [`AuthWire`] effect via [`crate::auth::apply_auth`] (the single
//! auth dispatch point), and this module only applies the effect — a header
//! (skipped when an enabled user header with the same name exists; the user's
//! header always wins) or a query pair appended after enabled params.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::auth::{AuthWire, apply_auth};
use crate::config::RedirectPolicy;
use crate::model::{BodyKind, Header, Method, Request, Response, Timing};

/// Default per-request timeout applied to the shared client; the config knob is
/// `timeout_secs` (see [`crate::config::Config::timeout`]).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default response body-size cap (10 MB); the config knob is `max_body_bytes`
/// (see [`crate::config::Config::max_body_bytes`]).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum number of redirect hops followed before giving up — matches
/// reqwest's default chain limit so `follow-all`/`strip` can't loop forever.
const MAX_REDIRECT_HOPS: usize = 10;

/// Set the first time a `follow-all` redirect is followed, so the foot-gun
/// warning fires exactly once per process. Also drives [`follow_all_warned`],
/// which the bin/tests use to observe that the warning fired.
static FOLLOW_ALL_WARNED: AtomicBool = AtomicBool::new(false);

/// Whether the one-time `follow-all` foot-gun warning has fired this process.
/// The `churl` bin can surface it on the UI; tests assert it fired.
pub fn follow_all_warned() -> bool {
    FOLLOW_ALL_WARNED.load(Ordering::Relaxed)
}

/// Per-execution knobs, resolved by the caller (config → defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// Maximum number of body bytes to read; the rest of the stream is dropped
    /// and the response is marked `truncated`.
    pub max_body_bytes: u64,
    /// How redirects are followed and whether auth-bearing headers survive a
    /// cross-origin hop. Defaults to [`RedirectPolicy::Strip`].
    pub redirect: RedirectPolicy,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            redirect: RedirectPolicy::default(),
        }
    }
}

/// Error executing an HTTP request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The request URL could not be parsed.
    #[error("invalid URL {url:?}: {reason}")]
    InvalidUrl {
        /// The offending URL string.
        url: String,
        /// Human-readable parse failure reason.
        reason: String,
    },
    /// The request timed out (client-side deadline elapsed).
    #[error("request timed out")]
    Timeout,
    /// The request failed for any other reason (connection, TLS, protocol, …).
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
}

/// Builds the shared [`reqwest::Client`]: rustls TLS, the given per-request
/// timeout ([`DEFAULT_TIMEOUT`] when nothing is configured), and a default
/// `User-Agent: churl/<version>` — some services reject UA-less requests
/// outright (e.g. httpbingo.org answers 402). An enabled user header named
/// `User-Agent` still wins per-request, like any other header. Redirects are
/// disabled at the client level ([`reqwest::redirect::Policy::none`]) and
/// followed manually in [`execute`] via [`follow_redirects`], so churl controls
/// exactly which headers survive a cross-origin hop.
pub fn build_client(timeout: Duration) -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .tls_backend_rustls()
        .user_agent(concat!("churl/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        // Redirects are followed manually in `execute` so churl controls exactly
        // which headers survive a cross-origin hop (see `follow_redirects`);
        // reqwest's built-in cross-origin stripping only covers a fixed five
        // headers and would miss a secret-named custom header. `none` hands us
        // every 3xx untouched.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(HttpError::Request)
}

/// Executes `request` on `client`, following redirects per
/// `options.redirect`, and returns the mapped [`Response`].
///
/// Only `enabled` headers and params are applied. Params are appended to the URL's
/// query string, preserving any query already present in the URL. When the request
/// carries a body, a `Content-Type` is derived from its [`BodyKind`] *unless* the
/// caller supplied an enabled `Content-Type` header — the user's header always wins.
/// Timing measures total wall-clock from just before send to the body being fully
/// read (or the cap being hit); connect timing is not split out and stays
/// `None`.
///
/// Redirects are followed manually (see [`follow_redirects`]) so churl controls
/// exactly which headers survive a cross-origin hop; under the default
/// [`RedirectPolicy::Strip`] every auth-bearing header is dropped when a hop
/// crosses the origin (scheme + host + port).
///
/// The body is streamed chunk-wise and accumulated up to `options.max_body_bytes`.
/// A chunk that would exceed the cap is cut at the cap boundary, the response is
/// marked `truncated`, and the rest of the stream is dropped — a runaway download
/// can never balloon memory past the cap.
pub async fn execute(
    client: &reqwest::Client,
    request: &Request,
    options: &ExecuteOptions,
) -> Result<Response, HttpError> {
    let auth_wire = request.auth.as_ref().map(apply_auth);
    let url = build_url(request, auth_wire.as_ref())?;
    let mut builder = client.request(reqwest_method(request.method), url);

    // Names of headers that carry auth material for THIS request — the NAME
    // anchor of the cross-origin strip set (`strip_auth_headers` also applies a
    // VALUE anchor at strip time, so a header need not be listed here to be
    // dropped if its value looks like a secret). `Authorization` and `Cookie`
    // are always in the set (standard credential-bearing headers); any
    // secret-named header the user set, and the churl-injected auth header, are
    // added below. Compared case-insensitively at strip time.
    let mut auth_header_names: Vec<String> = vec!["authorization".to_owned(), "cookie".to_owned()];

    let mut user_content_type = false;
    for header in &request.headers {
        if !header.enabled {
            continue;
        }
        if header.name.eq_ignore_ascii_case("content-type") {
            user_content_type = true;
        }
        if crate::config::looks_like_secret_name(&header.name) {
            auth_header_names.push(header.name.to_ascii_lowercase());
        }
        builder = builder.header(header.name.as_str(), header.value.as_str());
    }

    // Auth header injection, after user headers: injected only when no enabled
    // user header with the same name (case-insensitive) exists — the user's
    // header always wins. Query placement is handled in `build_url`.
    if let Some(AuthWire::Header { name, value }) = &auth_wire {
        let user_has_header = request
            .headers
            .iter()
            .any(|header| header.enabled && header.name.eq_ignore_ascii_case(name));
        if !user_has_header {
            builder = builder.header(name.as_str(), value.as_str());
            auth_header_names.push(name.to_ascii_lowercase());
        }
    }

    if let Some(body) = &request.body {
        if !user_content_type {
            builder = builder.header(reqwest::header::CONTENT_TYPE, content_type_for(body.kind));
        }
        builder = builder.body(body.content.clone().into_bytes());
    }

    let start = Instant::now();
    let initial = builder.build().map_err(map_send_error)?;
    let mut response = follow_redirects(client, initial, options.redirect, &auth_header_names)
        .await
        .map_err(map_send_error)?;
    let status = response.status().as_u16();
    let headers = collect_headers(response.headers());

    let cap = usize::try_from(options.max_body_bytes).unwrap_or(usize::MAX);
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.map_err(map_send_error)? {
        if body.len() + chunk.len() > cap {
            body.extend_from_slice(&chunk[..cap - body.len()]);
            truncated = true;
            break; // drop the response; the rest of the stream is never read
        }
        body.extend_from_slice(&chunk);
    }
    let total = start.elapsed();

    Ok(Response {
        status,
        headers,
        body,
        truncated,
        timing: Timing {
            connect: None,
            total,
        },
    })
}

/// Follows redirects manually so churl — not reqwest — decides exactly which
/// headers survive a cross-origin hop. The client is built with
/// `redirect::Policy::none`, so every 3xx lands here untouched.
///
/// Security predicate: a credential must never reach an origin the user didn't
/// send it to. Under [`RedirectPolicy::Strip`] (the default), when a hop's
/// target origin (scheme + host + port) differs from the current origin, a
/// header is dropped before following if EITHER its name is auth-bearing
/// (`auth_header_names`) OR its value *looks like* a secret
/// ([`crate::secrets::looks_like_secret_value`]) — see [`strip_auth_headers`].
/// The value anchor closes the gap where an opaque-named header carries a real
/// token (`X-Custom-Auth: sk-live-…`). reqwest's own stripping covers only a
/// fixed five header *names* and would miss both, so we do it ourselves.
/// [`RedirectPolicy::Strict`] refuses to follow a cross-origin hop at all and
/// surfaces the 3xx response. [`RedirectPolicy::FollowAll`] keeps every header
/// across origins (the foot-gun) and warns once.
///
/// Method/body rewriting matches reqwest/tower-http and RFC 9110: 303 → GET
/// (unless HEAD) and drop the body; 301/302 → POST becomes GET and drops the
/// body, other methods are untouched; 307/308 → method and body preserved.
async fn follow_redirects(
    client: &reqwest::Client,
    initial: reqwest::Request,
    policy: RedirectPolicy,
    auth_header_names: &[String],
) -> Result<reqwest::Response, reqwest::Error> {
    let mut current = initial;
    let mut hops = 0usize;
    loop {
        // Retain everything needed to build the next hop before `execute`
        // consumes `current`. A non-cloneable (streaming) body yields `None`;
        // we only need the clone for 307/308, handled below.
        let prev_url = current.url().clone();
        let prev_method = current.method().clone();
        let prev_headers = current.headers().clone();
        let retained_body = current.try_clone();

        let response = client.execute(current).await?;

        let status = response.status();
        // Resolve the next hop's URL, ending the borrow of `response` before any
        // early return. A non-3xx, a 3xx with no `Location`, or a
        // malformed/unparseable target all mean "nothing to follow" — surface
        // the response as-is. A relative `Location` resolves against the current
        // URL.
        let next_url = if status.is_redirection() {
            response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|loc| loc.to_str().ok())
                .and_then(|loc| prev_url.join(loc).ok())
        } else {
            None
        };
        let next_url = match next_url {
            Some(url) => url,
            None => return Ok(response),
        };

        // Hop cap: once we've already followed `MAX_REDIRECT_HOPS` redirects,
        // stop and surface this 3xx rather than following further — a redirect
        // loop can never spin forever.
        if hops >= MAX_REDIRECT_HOPS {
            return Ok(response);
        }
        hops += 1;

        let cross_origin = origin_differs(&prev_url, &next_url);

        match policy {
            RedirectPolicy::Strict if cross_origin => {
                // Surface the redirect instead of following it silently.
                return Ok(response);
            }
            RedirectPolicy::FollowAll if cross_origin => warn_follow_all_once(),
            _ => {}
        }

        // Build the next hop. Start from the retained request when the body must
        // be preserved (307/308); otherwise a fresh request with the carried
        // headers is enough.
        let (next_method, keep_body) = redirect_method(status, &prev_method);
        // The body is only actually carried when it was requested (307/308) AND
        // the request was cloneable. A non-cloneable body (streaming) yields
        // `None` and the hop proceeds bodyless — so we must also drop the
        // payload-describing headers, or a stale `Content-Length`/`Content-Type`
        // would misdescribe an empty body. (Unreachable today — churl bodies are
        // in-memory `Vec<u8>`, always cloneable — but correct if that changes.)
        let (mut next, body_carried) = match (keep_body, retained_body) {
            (true, Some(cloned)) => (cloned, true),
            _ => (
                reqwest::Request::new(next_method.clone(), next_url.clone()),
                false,
            ),
        };
        *next.method_mut() = next_method;
        *next.url_mut() = next_url.clone();
        *next.headers_mut() = prev_headers;
        if !body_carried {
            // A method downgrade (303, or POST on 301/302) or a dropped
            // non-cloneable body sends no body — so its payload-describing
            // headers go too, matching reqwest/tower-http.
            *next.body_mut() = None;
            drop_payload_headers(next.headers_mut());
        }

        // Strip credentials on a cross-origin hop under `strip`. `follow-all`
        // keeps them (already warned); `strict` never reaches here cross-origin.
        if cross_origin && matches!(policy, RedirectPolicy::Strip) {
            strip_auth_headers(next.headers_mut(), auth_header_names);
        }

        current = next;
    }
}

/// True when two URLs differ in scheme, host, or port — the origin boundary a
/// credential must not cross. Port comparison uses the scheme's default when
/// absent, so `http://h` (80) and `https://h` (443) correctly differ.
fn origin_differs(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() != b.scheme()
        || a.host_str() != b.host_str()
        || a.port_or_known_default() != b.port_or_known_default()
}

/// The method and body-preservation for the next hop given the redirect status
/// and the current method. Mirrors reqwest/tower-http: 303 → GET (unless HEAD),
/// drop body; 301/302 → POST becomes GET (drop body), other methods unchanged;
/// 307/308 → unchanged, keep body.
fn redirect_method(
    status: reqwest::StatusCode,
    method: &reqwest::Method,
) -> (reqwest::Method, bool) {
    match status {
        reqwest::StatusCode::SEE_OTHER => {
            if *method == reqwest::Method::HEAD {
                (reqwest::Method::HEAD, false)
            } else {
                (reqwest::Method::GET, false)
            }
        }
        reqwest::StatusCode::MOVED_PERMANENTLY | reqwest::StatusCode::FOUND => {
            if *method == reqwest::Method::POST {
                (reqwest::Method::GET, false)
            } else {
                (method.clone(), true)
            }
        }
        // 307/308 (and any other 3xx we chose to follow): preserve method + body.
        _ => (method.clone(), true),
    }
}

/// Removes every auth-bearing header before a cross-origin hop under `strip`,
/// via two anchors so a credential never reaches a foreign origin:
///
/// 1. **Name anchor** — the header's name (case-insensitive) is in `names`,
///    which always includes `authorization` and `cookie`, plus any secret-named
///    user header and the churl-injected auth header for this request.
/// 2. **Value anchor** — the header's value *looks like* a secret
///    ([`crate::secrets::looks_like_secret_value`]: `sk-`/`ghp_`/`AKIA` and
///    other vendor prefixes, JWTs, long high-entropy runs; a `{{placeholder}}`
///    is never secret-shaped). This catches an opaque-named credential header
///    such as `X-Custom-Auth: sk-live-…` that the name anchor alone misses.
///
/// Stripping is the safe direction: a value-shape false positive only means a
/// non-secret header doesn't reach a *different* origin. A whole header entry
/// (all its values) is removed when either anchor fires for any of its values.
fn strip_auth_headers(headers: &mut reqwest::header::HeaderMap, names: &[String]) {
    let mut to_remove: Vec<reqwest::header::HeaderName> = Vec::new();
    for (name, value) in headers.iter() {
        let name_hit = names.iter().any(|n| n.eq_ignore_ascii_case(name.as_str()));
        let value_hit = value
            .to_str()
            .is_ok_and(crate::secrets::looks_like_secret_value);
        if (name_hit || value_hit) && !to_remove.contains(name) {
            to_remove.push(name.clone());
        }
    }
    for name in to_remove {
        headers.remove(&name);
    }
}

/// Drops the payload-describing headers when a redirect changes the method and
/// drops the body (matches reqwest/tower-http's `drop_payload_headers`).
fn drop_payload_headers(headers: &mut reqwest::header::HeaderMap) {
    headers.remove(reqwest::header::CONTENT_TYPE);
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::CONTENT_ENCODING);
    headers.remove(reqwest::header::TRANSFER_ENCODING);
}

/// Emits the `follow-all` foot-gun warning exactly once per process and records
/// that it fired (observable via [`follow_all_warned`]).
fn warn_follow_all_once() {
    if !FOLLOW_ALL_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "warning: redirect = follow-all keeps auth headers across origins — a resolved secret can leak to a redirect target"
        );
    }
}

/// Parses the request URL and appends every enabled param as a query pair,
/// preserving any query already present in the URL string. A query-placed auth
/// effect is appended last, after the enabled params (no precedence rule: a
/// same-named user param and the auth pair are both sent).
fn build_url(request: &Request, auth_wire: Option<&AuthWire>) -> Result<reqwest::Url, HttpError> {
    let mut url = reqwest::Url::parse(&request.url).map_err(|err| HttpError::InvalidUrl {
        url: request.url.clone(),
        reason: err.to_string(),
    })?;
    let auth_query = match auth_wire {
        Some(AuthWire::Query { name, value }) => Some((name, value)),
        _ => None,
    };
    if request.params.iter().any(|param| param.enabled) || auth_query.is_some() {
        let mut pairs = url.query_pairs_mut();
        for param in &request.params {
            if param.enabled {
                pairs.append_pair(&param.name, &param.value);
            }
        }
        if let Some((name, value)) = auth_query {
            pairs.append_pair(name, value);
        }
    }
    Ok(url)
}

/// Maps a [`Method`] to reqwest's method type.
fn reqwest_method(method: Method) -> reqwest::Method {
    match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Patch => reqwest::Method::PATCH,
        Method::Delete => reqwest::Method::DELETE,
        Method::Head => reqwest::Method::HEAD,
        Method::Options => reqwest::Method::OPTIONS,
    }
}

/// The `Content-Type` derived from a body's [`BodyKind`].
fn content_type_for(kind: BodyKind) -> &'static str {
    match kind {
        BodyKind::Json => "application/json",
        BodyKind::Form => "application/x-www-form-urlencoded",
        BodyKind::Text => "text/plain",
    }
}

/// Collects response headers in wire order, lossily decoding non-UTF-8 values.
fn collect_headers(headers: &reqwest::header::HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(name, value)| Header {
            name: name.as_str().to_owned(),
            value: value
                .to_str()
                .map(str::to_owned)
                .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned()),
            enabled: true,
        })
        .collect()
}

/// Distinguishes a timeout from any other reqwest failure.
fn map_send_error(err: reqwest::Error) -> HttpError {
    if err.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Request(err)
    }
}
