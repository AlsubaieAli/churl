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

use std::time::{Duration, Instant};

use crate::auth::{AuthWire, apply_auth};
use crate::model::{BodyKind, Header, Method, Request, Response, Timing};

/// Default per-request timeout applied to the shared client; the config knob is
/// `timeout_secs` (see [`crate::config::Config::timeout`]).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default response body-size cap (10 MB); the config knob is `max_body_bytes`
/// (see [`crate::config::Config::max_body_bytes`]).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Per-execution knobs, resolved by the caller (config → defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// Maximum number of body bytes to read; the rest of the stream is dropped
    /// and the response is marked `truncated`.
    pub max_body_bytes: u64,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
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
/// timeout ([`DEFAULT_TIMEOUT`] when nothing is configured), reqwest's
/// default redirect policy (follow up to 10 hops), and a default
/// `User-Agent: churl/<version>` — some services reject UA-less requests
/// outright (e.g. httpbingo.org answers 402). An enabled user header named
/// `User-Agent` still wins per-request, like any other header.
pub fn build_client(timeout: Duration) -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .tls_backend_rustls()
        .user_agent(concat!("churl/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .map_err(HttpError::Request)
}

/// Executes `request` on `client`, returning the mapped [`Response`].
///
/// Only `enabled` headers and params are applied. Params are appended to the URL's
/// query string, preserving any query already present in the URL. When the request
/// carries a body, a `Content-Type` is derived from its [`BodyKind`] *unless* the
/// caller supplied an enabled `Content-Type` header — the user's header always wins.
/// Timing measures total wall-clock from just before send to the body being fully
/// read (or the cap being hit); connect timing is not split out and stays
/// `None`.
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

    let mut user_content_type = false;
    for header in &request.headers {
        if !header.enabled {
            continue;
        }
        if header.name.eq_ignore_ascii_case("content-type") {
            user_content_type = true;
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
        }
    }

    if let Some(body) = &request.body {
        if !user_content_type {
            builder = builder.header(reqwest::header::CONTENT_TYPE, content_type_for(body.kind));
        }
        builder = builder.body(body.content.clone().into_bytes());
    }

    let start = Instant::now();
    let mut response = builder.send().await.map_err(map_send_error)?;
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
