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

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::auth::{AuthWire, apply_auth};
use crate::config::RedirectPolicy;
use crate::cookies::ChurlCookieJar;
use crate::debug::DebugTrace;
use crate::model::{Body, BodyKind, Header, Method, Part, PartValue, Request, Response, Timing};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// Maximum number of body bytes to read; the rest of the stream is dropped
    /// and the response is marked `truncated`.
    pub max_body_bytes: u64,
    /// How redirects are followed and whether auth-bearing headers survive a
    /// cross-origin hop. Defaults to [`RedirectPolicy::Strip`].
    pub redirect: RedirectPolicy,
    /// The workspace root a relative [`crate::model::PartValue::File`] path
    /// resolves against (M8.6). Irrelevant for a request with no multipart
    /// file parts; the caller resolves this from the open workspace (falling
    /// back to `cwd` when none is open, matching `send`'s "workspace
    /// optional" contract) — see [`crate::model::Body::Multipart`].
    pub root: std::path::PathBuf,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            redirect: RedirectPolicy::default(),
            root: std::path::PathBuf::new(),
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
    /// A `multipart/form-data` file part (M8.6) could not be prepared: a
    /// missing/unreadable file, or a relative path that resolves outside the
    /// workspace root. Raised during the pre-flight Form build, strictly
    /// before `.send()` — so a bad file part fires **zero bytes** on the wire,
    /// never a half-multipart request.
    #[error("multipart part {name:?}: {reason}")]
    MultipartFile {
        /// The offending part's name.
        name: String,
        /// Human-readable failure reason (not-found, permission, traversal).
        reason: String,
    },
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
    build_client_with(&ClientConfig {
        timeout,
        ..ClientConfig::default()
    })
}

/// The session-scoped knobs that shape the shared client: timeout plus the three
/// M8 request controls (proxy, insecure-TLS, cookie jar). Every live change to any
/// of these takes effect by rebuilding the single client through
/// [`build_client_with`] — there is never more than one client, and it always
/// reflects current session state.
#[derive(Clone, Default)]
pub struct ClientConfig {
    /// Per-request timeout ([`DEFAULT_TIMEOUT`] when the field is left at its
    /// `Duration::ZERO` default → treated as "use the default").
    pub timeout: Duration,
    /// HTTP/HTTPS proxy URL. `Some` routes every request through
    /// `reqwest::Proxy::all`; `None` calls no `.proxy()` at all, so reqwest honors
    /// the `HTTP(S)_PROXY`/`NO_PROXY` environment for free.
    pub proxy: Option<String>,
    /// When `true`, disables TLS verification via
    /// `danger_accept_invalid_certs(true)` — with the rustls backend this also
    /// accepts hostname mismatches, so no separate hostname knob is needed (nor
    /// exists) here.
    pub insecure: bool,
    /// When `Some`, installs the jar as the client's cookie provider so cookies
    /// are stored/sent per hop with RFC 6265 origin scoping. The `Arc` is held on
    /// `App` and survives client rebuilds, so toggling off→on keeps the jar.
    pub cookies: Option<Arc<ChurlCookieJar>>,
}

/// Builds the shared client from a full [`ClientConfig`] — the single seam every
/// live proxy/insecure/cookies change routes through. `build_client` is the
/// timeout-only shortcut that leaves the three controls at their safe defaults
/// (no proxy, verify TLS, no jar).
///
/// Security predicates: `insecure = true` turns OFF certificate *and* hostname
/// verification (a loud RED indicator surfaces this in the UI); a `None` proxy
/// leaves env-proxy handling to reqwest rather than blanking it.
pub fn build_client_with(cfg: &ClientConfig) -> Result<reqwest::Client, HttpError> {
    let timeout = if cfg.timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        cfg.timeout
    };
    let mut builder = reqwest::Client::builder()
        .tls_backend_rustls()
        .user_agent(concat!("churl/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        // Redirects are followed manually in `execute` so churl controls exactly
        // which headers survive a cross-origin hop (see `follow_redirects`);
        // reqwest's built-in cross-origin stripping only covers a fixed five
        // headers and would miss a secret-named custom header. `none` hands us
        // every 3xx untouched.
        .redirect(reqwest::redirect::Policy::none());

    if let Some(proxy) = &cfg.proxy {
        // `Proxy::all` covers both HTTP and HTTPS targets. A malformed proxy URL
        // fails the build loudly rather than silently falling back to a direct
        // (unproxied) connection.
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(HttpError::Request)?);
    }
    // NB: when `cfg.proxy` is None we deliberately do NOT call `.proxy()`, so
    // reqwest keeps honoring `HTTP(S)_PROXY`/`NO_PROXY` from the environment.

    if cfg.insecure {
        // rustls: this single flag disables BOTH invalid-cert and hostname-mismatch
        // rejection, so `danger_accept_invalid_hostnames` (native-tls-oriented) is
        // neither added nor needed.
        builder = builder.danger_accept_invalid_certs(true);
    }

    if let Some(jar) = &cfg.cookies {
        builder = builder.cookie_provider(jar.clone());
    }

    builder.build().map_err(HttpError::Request)
}

/// Executes `request` on `client`, following redirects per
/// `options.redirect`, and returns the mapped [`Response`].
///
/// The `sink=None` shorthand of [`execute_traced`] — every normal send (no
/// debug capture) goes through here. See `execute_traced`'s docs for the full
/// behaviour; the two never diverge in what they send or return.
pub async fn execute(
    client: &reqwest::Client,
    request: &Request,
    options: &ExecuteOptions,
) -> Result<Response, HttpError> {
    execute_traced(client, request, options, None).await
}

/// Executes `request` on `client`, following redirects per
/// `options.redirect`, and returns the mapped [`Response`] — identically to
/// [`execute`], which is this function called with `sink: None`.
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
///
/// When `sink` is `Some`, every debug capture site along the way (the
/// masked/raw resolved request, the auth-injection decision, one
/// [`crate::debug::RedirectHop`] per redirect hop, and the mapped error cause
/// on failure) fills the trace — each site is gated behind
/// `sink.as_deref_mut()` being `Some`, so a `None` sink builds and pushes
/// nothing; this is the zero-overhead proof for [`execute`]'s normal path.
pub async fn execute_traced(
    client: &reqwest::Client,
    request: &Request,
    options: &ExecuteOptions,
    mut sink: Option<&mut DebugTrace>,
) -> Result<Response, HttpError> {
    if let Some(trace) = sink.as_deref_mut() {
        trace.capture_request(request);
    }

    let auth_wire = request.auth.as_ref().map(apply_auth);
    if let Some(AuthWire::Query { name, .. }) = &auth_wire
        && let Some(trace) = sink.as_deref_mut()
    {
        // Query-placed auth always appends (no user-header-wins override to
        // check, unlike the header case below), so the decision is final here.
        trace.decisions.auth_injected = Some(name.clone());
    }
    let url = build_url(request, auth_wire.as_ref()).inspect_err(|err| {
        if let Some(trace) = sink.as_deref_mut() {
            trace.record_error(err);
        }
    })?;
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
            if let Some(trace) = sink.as_deref_mut() {
                trace.decisions.auth_injected = Some(name.clone());
            }
        }
    }

    // A user-supplied Content-Type header always wins on a `Simple` body (the
    // header loop above already applied it, and we simply skip the derived
    // one). `.multipart()` below unconditionally *appends* its own
    // boundary-bearing Content-Type (reqwest's `header()` appends, not
    // replaces), so honouring a user override there takes a second step:
    // capture the value now and `HeaderMap::insert` (replace-not-append) it
    // back after `.build()`.
    let mut multipart_content_type_override: Option<String> = None;
    if let Some(body) = &request.body {
        match body {
            Body::Simple { kind, content } => {
                if !user_content_type {
                    builder =
                        builder.header(reqwest::header::CONTENT_TYPE, content_type_for(*kind));
                }
                builder = builder.body(content.clone().into_bytes());
            }
            Body::Multipart(parts) => {
                let form = build_multipart_form(parts, &options.root)
                    .await
                    .inspect_err(|err| {
                        if let Some(trace) = sink.as_deref_mut() {
                            trace.record_error(err);
                        }
                    })?;
                builder = builder.multipart(form);
                if user_content_type {
                    multipart_content_type_override = request
                        .headers
                        .iter()
                        .find(|header| {
                            header.enabled && header.name.eq_ignore_ascii_case("content-type")
                        })
                        .map(|header| header.value.clone());
                }
            }
        }
    }

    let start = Instant::now();
    let mut initial = builder.build().map_err(map_send_error).inspect_err(|err| {
        if let Some(trace) = sink.as_deref_mut() {
            trace.record_error(err);
        }
    })?;
    if let Some(value) = multipart_content_type_override
        && let Ok(header_value) = reqwest::header::HeaderValue::from_str(&value)
    {
        initial
            .headers_mut()
            .insert(reqwest::header::CONTENT_TYPE, header_value);
    }
    let mut response = follow_redirects(
        client,
        initial,
        options.redirect,
        &auth_header_names,
        sink.as_deref_mut(),
    )
    .await
    .map_err(map_send_error)
    .inspect_err(|err| {
        if let Some(trace) = sink.as_deref_mut() {
            trace.record_error(err);
        }
    })?;
    let status = response.status().as_u16();
    let headers = collect_headers(response.headers());

    let cap = usize::try_from(options.max_body_bytes).unwrap_or(usize::MAX);
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(map_send_error)
            .inspect_err(|err| {
                if let Some(trace) = sink.as_deref_mut() {
                    trace.record_error(err);
                }
            })?;
        let Some(chunk) = chunk else { break };
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

async fn follow_redirects(
    client: &reqwest::Client,
    initial: reqwest::Request,
    policy: RedirectPolicy,
    auth_header_names: &[String],
    mut sink: Option<&mut DebugTrace>,
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
        *next.method_mut() = next_method.clone();
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
        // The removed names are captured (by `strip_auth_headers`'s return, BEFORE
        // any further mutation) for the trace below, whether or not a sink is
        // attached — the cost is a `Vec` only ever populated when a header was
        // actually stripped, so the off-path (no cross-origin strip) allocates
        // nothing either way.
        let stripped_headers = if cross_origin && matches!(policy, RedirectPolicy::Strip) {
            strip_auth_headers(next.headers_mut(), auth_header_names)
        } else {
            Vec::new()
        };

        if let Some(trace) = sink.as_deref_mut() {
            let method_change = (next_method != prev_method)
                .then(|| (model_method(&prev_method), model_method(&next_method)));
            trace.redirect_hops.push(crate::debug::RedirectHop {
                from: crate::secrets::mask_url(prev_url.as_str()),
                to: crate::secrets::mask_url(next_url.as_str()),
                status: status.as_u16(),
                method_change,
                cross_origin,
                stripped_headers,
            });
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
///
/// Returns the names of the removed headers, captured before mutation, so a
/// caller (see [`follow_redirects`]'s [`crate::debug::RedirectHop`] capture)
/// can record exactly what was stripped without racing the removal itself.
fn strip_auth_headers(headers: &mut reqwest::header::HeaderMap, names: &[String]) -> Vec<String> {
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
    let removed: Vec<String> = to_remove
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect();
    for name in &to_remove {
        headers.remove(name);
    }
    removed
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

/// Maps a reqwest method back to our [`Method`] for [`crate::debug::RedirectHop`]'s
/// `method_change`. Every reqwest method reaching this call originates from our
/// own [`reqwest_method`] mapping (the initial request method, or a
/// [`redirect_method`] rewrite of it), so parsing back always succeeds; `GET` is
/// a defensive fallback only, never expected to trigger.
fn model_method(method: &reqwest::Method) -> Method {
    method.as_str().parse().unwrap_or(Method::Get)
}

/// The `Content-Type` derived from a body's [`BodyKind`].
fn content_type_for(kind: BodyKind) -> &'static str {
    match kind {
        BodyKind::Json => "application/json",
        BodyKind::Form => "application/x-www-form-urlencoded",
        BodyKind::Text => "text/plain",
    }
}

/// Builds a `reqwest` multipart [`Form`](reqwest::multipart::Form) from the
/// model's [`Part`]s (M8.6) — the pre-flight seam: every file part is
/// resolved, traversal-guarded, and `fstat`-ed here, entirely before the
/// caller sends anything. A missing/unreadable/out-of-guard file returns
/// [`HttpError::MultipartFile`] and the Form (hence the request) is never
/// built — zero bytes reach the wire for a bad part; nothing to unwind. Inline
/// text parts are buffered (`Part::text`); file parts stream from disk
/// (`Part::stream_with_length`, `len` from the pre-flight `fstat`) so a large
/// upload is never buffered whole in memory.
async fn build_multipart_form(
    parts: &[Part],
    root: &Path,
) -> Result<reqwest::multipart::Form, HttpError> {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        match &part.value {
            PartValue::Text(text) => {
                form = form.text(part.name.clone(), text.clone());
            }
            PartValue::File {
                path,
                filename,
                mime,
            } => {
                let resolved =
                    resolve_part_path(root, path).map_err(|reason| HttpError::MultipartFile {
                        name: part.name.clone(),
                        reason,
                    })?;
                let metadata = tokio::fs::metadata(&resolved).await.map_err(|err| {
                    HttpError::MultipartFile {
                        name: part.name.clone(),
                        reason: format!("{path}: {err}"),
                    }
                })?;
                let file = tokio::fs::File::open(&resolved).await.map_err(|err| {
                    HttpError::MultipartFile {
                        name: part.name.clone(),
                        reason: format!("{path}: {err}"),
                    }
                })?;
                let mut reqwest_part =
                    reqwest::multipart::Part::stream_with_length(file, metadata.len());
                // Default filename mirrors curl's own `-F name=@path` default
                // (the local file's basename) when no explicit `filename` is set.
                let display_name = filename.clone().unwrap_or_else(|| {
                    resolved
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                });
                reqwest_part = reqwest_part.file_name(display_name);
                if let Some(mime) = mime {
                    reqwest_part =
                        reqwest_part
                            .mime_str(mime)
                            .map_err(|err| HttpError::MultipartFile {
                                name: part.name.clone(),
                                reason: format!("invalid mime {mime:?}: {err}"),
                            })?;
                }
                form = form.part(part.name.clone(), reqwest_part);
            }
        }
    }
    Ok(form)
}

/// Resolves a multipart file part's `path` against `root` — the same
/// lexical-then-canonical traversal guard [`crate::sequence`]'s
/// `resolve_step_path` applies to sequence step endpoints, with one
/// deliberate difference: an **absolute** path is allowed through as-is (M8.6
/// locked decision — personal-tool convenience; not flagged here, the TUI
/// surfaces the non-portability). A relative path is lexically normalized
/// against `root` and rejected if it would escape (`..` climbing above the
/// root, or an in-workspace symlink pointing outside it). Resolution happens
/// here, at send time — never at import or save.
fn resolve_part_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let input = Path::new(path);
    if input.is_absolute() {
        return Ok(input.to_path_buf());
    }
    for component in input.components() {
        if matches!(component, Component::ParentDir) {
            return Err(format!("{path:?} escapes the workspace root"));
        }
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let joined = root.join(input);
    let normalized = lexical_normalize(&joined);
    if !normalized.starts_with(&root) {
        return Err(format!("{path:?} escapes the workspace root"));
    }
    // The lexical check above can be fooled by a symlinked component *inside*
    // root that points elsewhere; canonicalize the deepest existing ancestor
    // of the target and re-check it against root, so an in-workspace symlink
    // can't tunnel a relative path out.
    if part_path_escapes_root(&root, &normalized) {
        return Err(format!(
            "{path:?} escapes the workspace root (symlinked component)"
        ));
    }
    Ok(normalized)
}

/// Resolves `.`/`..` components without touching the filesystem (the target
/// usually does not exist yet, so `Path::canonicalize` cannot be used on it
/// directly). A leading `..` that would climb above the path root simply pops
/// nothing further, so an escaping path fails the later `starts_with` check.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` canonically resolves outside `root` (catches an
/// in-workspace symlink escape the lexical check above cannot see).
/// Canonicalizes the deepest existing ancestor of `path` — following
/// symlinks — and re-appends the not-yet-existing tail, then checks
/// containment. Conservative: returns `false` when nothing resolves (the
/// lexical pre-check already ran; a genuinely missing file fails later at the
/// `fstat` step with a distinct not-found reason).
fn part_path_escapes_root(root: &Path, path: &Path) -> bool {
    let mut ancestor = path;
    let mut tail = PathBuf::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            return !canonical.join(&tail).starts_with(root);
        }
        let Some(name) = ancestor.file_name() else {
            return false;
        };
        tail = Path::new(name).join(&tail);
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => return false,
        }
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
