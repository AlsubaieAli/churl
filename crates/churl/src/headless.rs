//! Shared machine-output payload types + the one execution seam `run` and
//! `send` both drive: build the [`churl_core::template::Resolver`] scopes,
//! substitute, refuse an unresolved `{{var}}` (fail loud, mirroring
//! `App::send_request`), build the client, call the SAME
//! `churl_core::http::execute` the TUI drives, then shape the frozen
//! `send`/`run` JSON payload (`docs/CLI.md`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use churl_core::assert::{Assertion, AssertionReport, run_assertions};
use churl_core::http::{ClientConfig, ExecuteOptions};
use churl_core::model::Request;
use churl_core::template::{Resolver, Scope};

use crate::output::{CliError, ErrorKind, SuccessExitCode, mask_header_value};

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
    /// `null` when no assertions were given (the M8.2 reserved shape,
    /// unchanged for an assertion-free call); populated with the
    /// [`AssertionReport`] otherwise. `report.passed == false` is a
    /// *successful* request/response — `ok`/`data` stay success-shaped — but
    /// still exits 1, the sole exception to "`ok` mirrors the exit code"
    /// (see [`SuccessExitCode`] and `docs/CLI.md`, "Assertions").
    pub assertions: Option<AssertionReport>,
}

impl SuccessExitCode for ExecData {
    /// Exit 1 iff assertions were given and at least one failed; 0 otherwise
    /// (including the assertion-free case) — the request itself already
    /// succeeded by the time `ExecData` exists (a transport/resolution error
    /// short-circuits earlier as a [`CliError`], so no assertions ever run
    /// against a response that doesn't exist).
    fn success_exit_code(&self) -> i32 {
        match &self.assertions {
            Some(report) if !report.passed => 1,
            _ => 0,
        }
    }
}

/// Knobs that shape the client + execution, resolved by the caller from CLI
/// globals (`--proxy`/`-k`) and the global config — mirrors
/// `App::install_runtime`/`App::rebuild_client` minus the session-lifetime
/// caching (a one-shot process builds exactly one client and exits).
pub struct ExecInputs {
    pub client_cfg: ClientConfig,
    pub exec_opts: ExecuteOptions,
}

/// Parses every `--assert` flag via [`Assertion::parse`], mapping the first
/// failure onto [`ErrorKind::InvalidAssertion`] (band 5 — a bad `--assert` is
/// a usage/input mistake, never a runtime request failure). Shared by `run`
/// (appended after the endpoint's persisted set) and `send` (its only
/// source of assertions).
pub fn parse_cli_assertions(exprs: &[String]) -> Result<Vec<Assertion>, CliError> {
    exprs
        .iter()
        .map(|expr| {
            Assertion::parse(expr).map_err(|err| {
                CliError::with_detail(
                    ErrorKind::InvalidAssertion,
                    format!("invalid --assert {expr:?}: {err}"),
                    serde_json::json!({ "assert": expr }),
                )
            })
        })
        .collect()
}

/// Resolves `{{var}}` placeholders over `scopes` (highest precedence first,
/// env implicit last — see [`Resolver`]), refuses an unresolved placeholder,
/// executes via the shared core executor, evaluates `assertions` against the
/// response, and shapes the result into the frozen envelope payload. `scopes`
/// excludes the TUI's "session" scope (no live multi-request session exists
/// in a one-shot headless process).
pub async fn run_execution(
    mut request: Request,
    scopes: Vec<Scope>,
    inputs: ExecInputs,
    assertions: &[Assertion],
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

    // Evaluated here — the last point `response` is still owned whole, before
    // its body is consumed into the (lossy-decoded) envelope projection below.
    // An empty set stays `None` (the M8.2 back-compat contract for an
    // assertion-free call), never a vacuously-passing empty report object.
    let assertion_report = if assertions.is_empty() {
        None
    } else {
        Some(run_assertions(assertions, &response))
    };

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
        assertions: assertion_report,
    })
}

/// Prints a compact human-mode rendering of a successful [`ExecData`]: the
/// response body on stdout (curl-like, so `churl send URL | jq` works), with
/// a request/response debug trace on stderr when `verbose`, followed by an
/// assertions checklist on stderr when any were given.
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

    // The assertions checklist is a distinct, always-stderr surface (never
    // mixed into the stdout body an agent/script may be piping) — printed
    // whenever any assertion ran, `verbose` or not.
    if let Some(report) = &data.assertions {
        for r in &report.results {
            let mark = if r.pass { "✓" } else { "✗" };
            let mut line = format!("{mark} {} {}", r.target, r.op);
            if let Some(expected) = &r.expected {
                line.push_str(&format!(" {expected}"));
            }
            if let Some(err) = &r.error {
                line.push_str(&format!(" — {err}"));
            }
            eprintln!("{line}");
        }
        eprintln!(
            "{} passed, {} failed",
            report.total - report.failed,
            report.failed
        );
    }
}
