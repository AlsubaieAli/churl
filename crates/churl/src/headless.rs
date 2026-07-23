//! Shared machine-output payload types + the one execution seam `run` and
//! `send` both drive: build the [`churl_core::template::Resolver`] scopes,
//! substitute, refuse an unresolved `{{var}}` (fail loud, mirroring
//! `App::send_request`), build the client, call the SAME
//! `churl_core::http::execute` the TUI drives, then shape the frozen
//! `send`/`run` JSON payload (`docs/CLI.md`).

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use churl_core::assert::{Assertion, AssertionReport, run_assertions};
use churl_core::debug::DebugTrace;
use churl_core::http::{ClientConfig, ExecuteOptions};
use churl_core::model::{Request, Response};
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
    /// M8.6, additive (`schema_version` stays 1): the part count of a
    /// `multipart/form-data` body, `None` for anything else (no body, or a
    /// `Simple` one — `body_present` already covers those). Omitted from the
    /// JSON envelope when `None` so a non-multipart `send`/`run` payload is
    /// byte-identical to before this field existed. File-part contents are
    /// never echoed — only the count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_parts: Option<usize>,
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
    /// The per-exchange debug trace, `Some` only when `-v/--verbose` was
    /// given (see [`run_execution`]'s `capture` flag). `None` on every
    /// ordinary invocation, and omitted from `--json` output entirely (never
    /// emitted as `"trace": null`) in that case, so the non-verbose envelope
    /// stays byte-identical to before this field existed. Additive per the
    /// `SCHEMA_VERSION` bump rule (`docs/CLI.md`, `output.rs` module docs):
    /// an omitted-when-absent optional field never bumps the schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<DebugTrace>,
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
///
/// `capture` gates a per-exchange [`DebugTrace`]: `true` only when
/// `-v/--verbose` was given on this invocation (independent of the
/// persisted `config.debug` knob — see `docs/CLI.md`). `false` takes the
/// exact pre-M8.3 path (`substitute_request` + `execute`, no `DebugTrace`
/// ever built); `true` swaps in the traced twins
/// (`substitute_request_traced` + `execute_traced`) and fills
/// `ExecData::trace`.
///
/// `output` (M8.7, `-o/--output`) writes the RAW response body bytes to disk
/// (or stdout for `-`) right here — this is the one place the actual
/// `Response` is in hand, before `shape_exec_data` projects its body into the
/// envelope's lossy-free utf8/base64 shape. `cwd` resolves a relative
/// `output` path (curl `-o`-style; never the workspace root, which may differ
/// from `cwd` for `run`). A write failure surfaces as its own `CliError`
/// (`OutputWriteFailed`) even though the request itself succeeded. When the
/// body was capped by `max_body_bytes`, `-o` still writes what it has (curl
/// semantics — a partial save, not a failure) but a loud stderr warning
/// accompanies it (M8.7.1), independent of `-v`, so `-o` never *silently*
/// produces a truncated file.
pub async fn run_execution(
    mut request: Request,
    scopes: Vec<Scope>,
    inputs: ExecInputs,
    assertions: &[Assertion],
    capture: bool,
    output: Option<&Path>,
    cwd: &Path,
) -> Result<ExecData, CliError> {
    let resolver = Resolver::new(scopes);

    // A capture sink is built only when `capture` asked for one — the
    // non-verbose path never constructs a `DebugTrace` at all, matching the
    // zero-overhead discipline `churl_core::http::execute_traced` documents.
    let mut trace = capture.then(|| DebugTrace::from_request(&request));
    match trace.as_mut() {
        Some(trace) => resolver.substitute_request_traced(&mut request, &mut trace.var_steps),
        None => resolver.substitute_request(&mut request),
    }

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

    let client = churl_core::http::build_client_with(&inputs.client_cfg)
        .map_err(crate::output::from_http_error)?;
    let response =
        churl_core::http::execute_traced(&client, &request, &inputs.exec_opts, trace.as_mut())
            .await
            .map_err(crate::output::from_http_error)?;

    // `execute_traced` has no access to the `ClientConfig` used to build
    // `client` (see `AuthCookieProxyDecisions`'s docs) — this is the one
    // place both the trace and `inputs.client_cfg` are in scope, so the
    // cookie/proxy decisions are filled in here, after a successful send.
    if let Some(trace) = trace.as_mut() {
        trace.decisions.cookie_used = inputs.client_cfg.cookies.is_some();
        trace.decisions.proxy = inputs
            .client_cfg
            .proxy
            .as_deref()
            .map(churl_core::secrets::mask_url);
    }

    if let Some(path) = output {
        write_response_body(path, cwd, &response.body)?;
        // Loud and independent of `-v`/`--json` (M8.7.1): `-o` silently
        // wrote only the first `max_body_bytes` of the body — the same
        // failure mode the TUI's save-response-body warns about. stderr
        // keeps the `--json` stdout contract clean (envelope-only).
        if response.truncated {
            eprintln!(
                "warning: saved {} bytes (response was truncated at max_body_bytes)",
                response.body.len()
            );
        }
    }

    Ok(shape_exec_data(&request, &response, assertions, trace))
}

/// Writes `-o/--output`'s raw response bytes to disk (or stdout for `-`),
/// byte-exact — no lossy utf8/base64 round-trip through the envelope shape
/// (the M8.2 base64-on-binary-stdout behaviour `-o` fixes). curl `-o`-style: a
/// relative path resolves against `cwd` (the process cwd — a download
/// destination, not a workspace artifact, so `run`'s workspace root is
/// deliberately NOT used here); an absolute path is honored as-is. Parent
/// directories are created as needed; the actual file write is atomic
/// (temp+fsync+rename) via `churl_core::persistence::atomic_write`.
fn write_response_body(path: &Path, cwd: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if path.as_os_str() == "-" {
        use std::io::Write as _;
        return std::io::stdout().write_all(bytes).map_err(|err| {
            CliError::new(
                ErrorKind::OutputWriteFailed,
                format!("failed to write response body to stdout: {err}"),
            )
        });
    }
    let target = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|err| {
            CliError::new(
                ErrorKind::OutputWriteFailed,
                format!("failed to create directory {}: {err}", parent.display()),
            )
        })?;
    }
    churl_core::persistence::atomic_write(&target, bytes).map_err(|err| {
        CliError::new(
            ErrorKind::OutputWriteFailed,
            format!("failed to write output file {}: {err}", target.display()),
        )
    })
}

/// Shapes a completed exchange into the frozen `data` payload: the masked
/// request echo, the response projection (UTF-8 text or base64), the
/// assertion report (`None` for an empty set — the M8.2 back-compat contract,
/// never a vacuously-passing empty object), and the optional `-v` debug
/// trace. Pure — no IO, no async.
///
/// The single source of truth for the per-request envelope shape: both the
/// single-request `run`/`send` path ([`run_execution`]) and the per-step
/// headless sequence runner ([`crate::seq_cmd`]) call it, so a step of a
/// sequence and a standalone `run` of the same endpoint emit byte-identical
/// `data`. Keeping the request-echo secret masking here (not duplicated per
/// caller) is deliberate: it is a security surface (`docs/CLI.md`, "Secret
/// masking").
pub fn shape_exec_data(
    request: &Request,
    response: &Response,
    assertions: &[Assertion],
    trace: Option<DebugTrace>,
) -> ExecData {
    let request_summary = RequestSummary {
        method: request.method.to_string(),
        // Mask any secret the resolver substituted into the URL (userinfo
        // password, secret-named/secret-shaped query value) before it is echoed
        // back — the real request that was sent used the unmasked `request.url`.
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
        body_present: request.body.is_some(),
        body_parts: match &request.body {
            Some(churl_core::model::Body::Multipart(parts)) => Some(parts.len()),
            _ => None,
        },
    };

    // An empty set stays `None` (the M8.2 back-compat contract for an
    // assertion-free call), never a vacuously-passing empty report object.
    let assertion_report = if assertions.is_empty() {
        None
    } else {
        Some(run_assertions(assertions, response))
    };

    let (body, body_encoding) = match String::from_utf8(response.body.clone()) {
        // Deterministic decoding for agents: valid UTF-8 ships as text, never
        // `to_string_lossy` (which would silently mangle non-UTF-8 bytes).
        Ok(text) => (text, "utf8"),
        Err(_) => (BASE64.encode(&response.body), "base64"),
    };

    ExecData {
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
        trace,
    }
}

/// Prints a compact human-mode rendering of a successful [`ExecData`]: the
/// response body on stdout (curl-like, so `churl send URL | jq` works), with
/// a request/response debug trace on stderr when `verbose`, followed by an
/// assertions checklist on stderr when any were given.
///
/// `suppress_body` (M8.7.1) is true iff `-o -` was given: `run_execution`
/// already wrote the raw body to stdout in that case, so the default human
/// body echo below is skipped — otherwise the body would print twice (curl's
/// own `-o -` writes it exactly once).
pub fn print_human(data: &ExecData, verbose: bool, suppress_body: bool) {
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
    // `-o -` (M8.7.1) already wrote the body to stdout once, in
    // `run_execution` — never echo it a second time here (curl's own `-o -`
    // writes the body exactly once).
    if !suppress_body {
        print!("{}", data.response.body);
    }

    // The assertions checklist is a distinct, always-stderr surface (never
    // mixed into the stdout body an agent/script may be piping) — printed
    // whenever any assertion ran, `verbose` or not.
    if let Some(report) = &data.assertions {
        print_assertion_checklist(report);
    }
}

/// Prints an assertion report as a human checklist on **stderr**: one `✓`/`✗`
/// line per result (`target op [expected]`, with the failure reason appended
/// after `✗`), then a `N passed, M failed` summary. Shared by every headless
/// command that surfaces assertions in human mode — `run`/`send` (via
/// [`print_human`]) and `load` — so their assertion rendering can't drift.
pub fn print_assertion_checklist(report: &AssertionReport) {
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
