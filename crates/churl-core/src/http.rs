//! HTTP request execution via `reqwest` + `rustls`.
//!
//! This module is deliberately runtime-agnostic: [`execute`] is a plain `async fn`
//! with no [`tokio`](https://docs.rs/tokio) types in its signature. Cancellation is
//! handled at the *task* level by the caller — the TUI spawns the future with
//! `tokio::spawn` and keeps the resulting `AbortHandle` — so `churl-core` never needs
//! to know about the runtime. There is no `{{var}}` templating here; URLs, headers,
//! and body are used verbatim (templating arrives in M5).

use std::time::{Duration, Instant};

use crate::model::{BodyKind, Header, Method, Request, Response, Timing};

/// Default per-request timeout applied to the shared client.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Error executing an HTTP request.
#[derive(Debug, thiserror::Error)]
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

/// Builds the shared [`reqwest::Client`]: rustls TLS, a 30 s default timeout, and
/// reqwest's default redirect policy (follow up to 10 hops).
pub fn build_client() -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .tls_backend_rustls()
        .timeout(DEFAULT_TIMEOUT)
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
/// read; connect timing is not split out in M3 and stays `None`.
pub async fn execute(client: &reqwest::Client, request: &Request) -> Result<Response, HttpError> {
    let url = build_url(request)?;
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

    if let Some(body) = &request.body {
        if !user_content_type {
            builder = builder.header(reqwest::header::CONTENT_TYPE, content_type_for(body.kind));
        }
        builder = builder.body(body.content.clone().into_bytes());
    }

    let start = Instant::now();
    let response = builder.send().await.map_err(map_send_error)?;
    let status = response.status().as_u16();
    let headers = collect_headers(response.headers());
    let body = response.bytes().await.map_err(map_send_error)?.to_vec();
    let total = start.elapsed();

    Ok(Response {
        status,
        headers,
        body,
        timing: Timing {
            connect: None,
            total,
        },
    })
}

/// Parses the request URL and appends every enabled param as a query pair,
/// preserving any query already present in the URL string.
fn build_url(request: &Request) -> Result<reqwest::Url, HttpError> {
    let mut url = reqwest::Url::parse(&request.url).map_err(|err| HttpError::InvalidUrl {
        url: request.url.clone(),
        reason: err.to_string(),
    })?;
    if request.params.iter().any(|param| param.enabled) {
        let mut pairs = url.query_pairs_mut();
        for param in &request.params {
            if param.enabled {
                pairs.append_pair(&param.name, &param.value);
            }
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
