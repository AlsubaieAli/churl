//! Shared machine-output payload types + the one execution seam `run` and
//! `send` both drive: build the [`churl_core::template::Resolver`] scopes,
//! substitute, refuse an unresolved `{{var}}` (fail loud, mirroring
//! `App::send_request`), build the client, call the SAME
//! `churl_core::http::execute` the TUI drives, then shape the frozen
//! `send`/`run` JSON payload (`docs/CLI.md`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use churl_core::http::{ClientConfig, ExecuteOptions};
use churl_core::model::Request;
use churl_core::template::{Resolver, Scope};

use crate::output::{CliError, ErrorKind, mask_header_value};

/// One request/response header line in the envelope.
#[derive(Debug, Serialize)]
pub struct HeaderPair {
    pub name: String,
    pub value: String,
}

/// The echoed `request` half of the `send`/`run` payload. Reflects exactly
/// what was sent (enabled headers only), with any auth-bearing header value
/// masked — see [`mask_header_value`] — so a resolved session-captured secret
/// never round-trips back out over stdout.
#[derive(Debug, Serialize)]
pub struct RequestSummary {
    pub method: String,
    pub url: String,
    pub headers: Vec<HeaderPair>,
    pub body_present: bool,
}

/// Coarse timing, milliseconds — the JSON-friendly projection of
/// `churl_core::model::Timing`.
#[derive(Debug, Serialize)]
pub struct TimingMs {
    pub total: u128,
}

/// The echoed `response` half of the `send`/`run` payload.
#[derive(Debug, Serialize)]
pub struct ResponseSummary {
    pub status: u16,
    pub headers: Vec<HeaderPair>,
    pub body: String,
    pub body_encoding: &'static str,
    pub truncated: bool,
    pub timing_ms: TimingMs,
}

/// The full `data` payload of a `send`/`run` envelope.
#[derive(Debug, Serialize)]
pub struct ExecData {
    pub request: RequestSummary,
    pub response: ResponseSummary,
    /// RESERVED for M8.4 assertions — always `null` in M8.2. Shipping the key
    /// now avoids a schema-version bump when assertions land.
    pub assertions: Option<serde_json::Value>,
}

/// Knobs that shape the client + execution, resolved by the caller from CLI
/// globals (`--proxy`/`-k`) and the global config — mirrors
/// `App::install_runtime`/`App::rebuild_client` minus the session-lifetime
/// caching (a one-shot process builds exactly one client and exits).
pub struct ExecInputs {
    pub client_cfg: ClientConfig,
    pub exec_opts: ExecuteOptions,
}

/// Resolves `{{var}}` placeholders over `scopes` (highest precedence first,
/// env implicit last — see [`Resolver`]), refuses an unresolved placeholder,
/// executes via the shared core executor, and shapes the result into the
/// frozen envelope payload. `scopes` excludes the TUI's "session" scope (no
/// live multi-request session exists in a one-shot headless process).
pub async fn run_execution(
    mut request: Request,
    scopes: Vec<Scope>,
    inputs: ExecInputs,
) -> Result<ExecData, CliError> {
    let resolver = Resolver::new(scopes);
    resolver.substitute_request(&mut request);

    // Fail loud (mirrors `App::send_request`): a literal `{{name}}` left in
    // the wire request is always a bug in the caller's vars, never something
    // to ship silently.
    let unresolved = churl_core::template::unresolved_placeholders(&request);
    if !unresolved.is_empty() {
        return Err(CliError::with_detail(
            ErrorKind::UnresolvedVar,
            format!(
                "unresolved variable(s), no scope (nor process env) resolved: {}",
                unresolved.join(", ")
            ),
            serde_json::json!({ "vars": unresolved }),
        ));
    }

    let body_present = request.body.is_some();
    let request_summary = RequestSummary {
        method: request.method.to_string(),
        // Mask any secret the resolver substituted into the URL (userinfo
        // password, secret-named/secret-shaped query value) before it is echoed
        // back — the real request below still uses the unmasked `request.url`.
        url: churl_core::secrets::mask_url(&request.url),
        headers: request
            .headers
            .iter()
            .filter(|h| h.enabled)
            .map(|h| HeaderPair {
                name: h.name.clone(),
                value: mask_header_value(&h.name, &h.value),
            })
            .collect(),
        body_present,
    };

    let client = churl_core::http::build_client_with(&inputs.client_cfg)
        .map_err(crate::output::from_http_error)?;
    let response = churl_core::http::execute(&client, &request, &inputs.exec_opts)
        .await
        .map_err(crate::output::from_http_error)?;

    let (body, body_encoding) = match String::from_utf8(response.body.clone()) {
        // Deterministic decoding for agents: valid UTF-8 ships as text, never
        // `to_string_lossy` (which would silently mangle non-UTF-8 bytes).
        Ok(text) => (text, "utf8"),
        Err(_) => (BASE64.encode(&response.body), "base64"),
    };

    Ok(ExecData {
        request: request_summary,
        response: ResponseSummary {
            status: response.status,
            headers: response
                .headers
                .iter()
                .map(|h| HeaderPair {
                    name: h.name.clone(),
                    value: h.value.clone(),
                })
                .collect(),
            body,
            body_encoding,
            truncated: response.truncated,
            timing_ms: TimingMs {
                total: response.timing.total.as_millis(),
            },
        },
        assertions: None,
    })
}

/// Prints a compact human-mode rendering of a successful [`ExecData`]: the
/// response body on stdout (curl-like, so `churl send URL | jq` works), with
/// a request/response debug trace on stderr when `verbose`.
pub fn print_human(data: &ExecData, verbose: bool) {
    if verbose {
        eprintln!("> {} {}", data.request.method, data.request.url);
        for h in &data.request.headers {
            eprintln!("> {}: {}", h.name, h.value);
        }
        eprintln!("< {}", data.response.status);
        for h in &data.response.headers {
            eprintln!("< {}: {}", h.name, h.value);
        }
        eprintln!(
            "< {} ms{}",
            data.response.timing_ms.total,
            if data.response.truncated {
                " (truncated)"
            } else {
                ""
            }
        );
    }
    print!("{}", data.response.body);
}
