//! `churl run-seq <name>` — headless request-sequence run (M8.4.1).
//!
//! Runs a saved `sequences/<name>.toml` end-to-end with no TUI, gating each
//! step against its endpoint's persisted `[[assertions]]` (M8.4). It reuses the
//! *exact* sequence engine the live TUI runner drives
//! ([`churl_core::sequence`]: `ordered_steps` → `prepare_step` → `classify_step`
//! → `should_halt`), so an interactively-authored sequence runs byte-identically
//! headless. Extracted values chain across steps through the in-process
//! `extracted` accumulator — one process replaces the fragile shell-script
//! `jq`-extract + `--var`-reinject-per-hop workaround (and, being one process,
//! that chain never touches disk).
//!
//! # Output contract (freeze-once — `docs/CLI.md`, "Sequence & load runs")
//!
//! In `--json` mode the run is an **NDJSON stream on stdout**: one object per
//! line, each self-contained (independently parseable — the NDJSON ideal).
//!
//! - One **step line** per step (`"type":"step"`): the same
//!   `{schema_version, ok, command, data, error}` shape a single-request
//!   `run`/`send` envelope carries — `data` is the *identical* frozen
//!   [`ExecData`] (via [`shape_exec_data`]) — plus a `"type"` discriminator, the
//!   step's `seq`/`endpoint`, and a `"skipped"` marker. `ok` mirrors the
//!   single-request rule: `true` for any completed request (an HTTP error
//!   *status* is still `ok:true` — assert on `data.response.status` to catch
//!   it); `false` only for a transport/resolution error (`data:null`,
//!   `error` populated) or a halted-tail skip (`skipped:true`).
//! - One terminal **summary line** (`"type":"summary"`): the freeze-once run
//!   rollup — overall `ok`, per-step counts, and the assertion totals across
//!   every step.
//!
//! **Exit code** mirrors the single-request precedence ("request/transport
//! errors still win"): the first hard step error's band (3/4/5) if any step
//! failed to prepare or send; otherwise **1** if any assertion failed *or* any
//! step's extraction chain broke (`extract_error`); else 0. An assertion or
//! extraction failure never flips a step's `ok` (the request still completed) —
//! only the exit code, exactly as an assertion failure does in single-request
//! mode. A completed HTTP error *status* (≥400) is not itself an exit trigger
//! (assert `status < 400` to gate it), consistent with single-request `run`.
//!
//! Human (non-`--json`) mode prints a compact per-step checklist and a summary
//! line to **stderr** (stdout stays empty — a multi-step run has no single body
//! to emit), mirroring the single-request assertion checklist.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use churl_core::debug::DebugTrace;
use churl_core::http::{ClientConfig, ExecuteOptions, build_client_with};
use churl_core::sequence::{
    RunScopes, SequenceError, StepResult, classify_step, ordered_steps, prepare_step, should_halt,
};

use crate::headless::{ExecData, shape_exec_data};
use crate::output::{CliError, EnvelopeError, ErrorKind, SCHEMA_VERSION, emit_error};
use crate::resolve::resolve_profile_vars;

/// The subcommand name stamped into every envelope/line's `command` field.
const COMMAND: &str = "run-seq";

/// CLI-sourced inputs to a headless sequence run (the process-wide
/// [`crate::RuntimeCfg`] carries the global-config-sourced knobs).
pub struct SeqArgs {
    /// The sequence's file stem: `run-seq checkout` runs `sequences/checkout.toml`.
    pub name: String,
    pub cli_vars: BTreeMap<String, String>,
    pub profile: Option<String>,
    pub proxy: Option<String>,
    pub insecure: bool,
    /// `-v`/`--verbose`: adds a `data.trace` object to each step's envelope
    /// under `--json` (secrets masked), mirroring single-request `run -v`.
    pub verbose: bool,
}

/// One NDJSON line per step — executed or halted-tail-skipped.
#[derive(Serialize)]
struct StepLine<'a> {
    schema_version: u32,
    #[serde(rename = "type")]
    line_type: &'static str,
    command: &'a str,
    /// The step's ordering key (`seq`), stable across runs.
    seq: u32,
    /// The step's endpoint path string (e.g. `auth/login.toml`).
    endpoint: &'a str,
    /// `true` for any completed request (see the module contract); `false` on a
    /// hard error or a skip.
    ok: bool,
    /// `true` only for a step that never ran because an earlier step halted the
    /// sequence. Omitted (not `false`) otherwise, so a normal step line stays
    /// close to the single-request envelope shape.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    skipped: bool,
    /// The frozen single-request payload on a completed request; `null` on a
    /// hard error or a skip.
    data: Option<ExecData>,
    /// Populated only on a hard (transport/resolution) step error.
    error: Option<EnvelopeError>,
    /// Present when the request completed (`ok:true`, `data` populated) but one
    /// of the step's `extract` rules failed to capture its value — the chain is
    /// broken. Distinct from `error` (a transport failure, `data:null`): the
    /// request itself succeeded, so `ok` stays `true`, but the run fails
    /// (exit 1), because a downstream step that needs the captured value can
    /// never resolve it. Omitted when extraction succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    extract_error: Option<String>,
}

/// The terminal NDJSON line: the freeze-once run summary.
#[derive(Serialize)]
struct SummaryLine<'a> {
    schema_version: u32,
    #[serde(rename = "type")]
    line_type: &'static str,
    command: &'a str,
    /// `true` ⟺ exit 0: no hard step error and no failed assertion.
    ok: bool,
    /// The run's sequence name (its file stem).
    sequence: &'a str,
    steps: StepStats,
    assertions: AssertTotals,
}

/// Per-step tallies for the summary line.
#[derive(Serialize, Default)]
struct StepStats {
    /// Every step in the sequence.
    total: usize,
    /// Steps actually attempted (not halted-tail-skipped) — includes hard
    /// failures.
    ran: usize,
    /// Steps skipped because an earlier step halted the run.
    skipped: usize,
    /// Attempted steps that hit a hard (transport/resolution) error — *not*
    /// assertion failures (those live in [`AssertTotals`]) and *not* HTTP error
    /// statuses (a completed 4xx/5xx is not a hard failure).
    failed: usize,
}

/// Assertion tallies aggregated across every step's report.
#[derive(Serialize, Default)]
struct AssertTotals {
    total: usize,
    passed: usize,
    failed: usize,
}

/// Runs the named sequence headlessly, printing the NDJSON stream (or the human
/// checklist) and returning the process exit code. Pre-flight failures (no
/// workspace, sequence not found, bad `--profile`) surface as a single error
/// envelope — the stream has not started — via [`emit_error`]; per-step
/// failures ride the stream.
pub async fn run(args: SeqArgs, cwd: &Path, runtime: &crate::RuntimeCfg, json: bool) -> i32 {
    // --- pre-flight: open the workspace and resolve the named sequence ---
    let workspace = match churl::tui::app::open_workspace(cwd) {
        Ok(Some(ws)) => ws,
        Ok(None) => {
            return emit_error(
                COMMAND,
                json,
                CliError::new(
                    ErrorKind::NoWorkspace,
                    format!(
                        "no churl workspace at {} (no churl.toml) — run `churl init` first",
                        cwd.display()
                    ),
                ),
            );
        }
        Err(err) => {
            return emit_error(
                COMMAND,
                json,
                CliError::new(ErrorKind::NoWorkspace, err.to_string()),
            );
        }
    };

    let loaded = match workspace.sequences() {
        Ok(loaded) => loaded,
        Err(err) => {
            return emit_error(
                COMMAND,
                json,
                CliError::new(
                    ErrorKind::SequenceNotFound,
                    format!("failed to read sequences/: {err}"),
                ),
            );
        }
    };
    // A single unparseable *sibling* sequence file degrades to a warning (the
    // lenient loader posture) — never silently swallowed.
    for warning in &loaded.warnings {
        eprintln!("warning: {warning}");
    }
    // Address by file stem (the stable identifier — naming law), never by the
    // human `name` field: `run-seq checkout` runs `sequences/checkout.toml`.
    // Enumerating real files (rather than joining the arg into a path) also
    // makes a `..`/absolute `<name>` a plain not-found, never a traversal.
    let Some((_, sequence)) = loaded
        .sequences
        .into_iter()
        .find(|(path, _)| path.file_stem().and_then(|s| s.to_str()) == Some(args.name.as_str()))
    else {
        return emit_error(
            COMMAND,
            json,
            CliError::with_detail(
                ErrorKind::SequenceNotFound,
                format!(
                    "no sequence {:?} (looked for sequences/{}.toml)",
                    args.name, args.name
                ),
                serde_json::json!({ "sequence": args.name }),
            ),
        );
    };

    let profile_vars = match resolve_profile_vars(Some(&workspace), args.profile.as_deref()) {
        Ok(vars) => vars,
        Err(err) => return emit_error(COMMAND, json, err),
    };

    // Ambient scopes below the ephemeral `extracted` chain. No live "session"
    // scope — a one-shot process never captures one; extracted values chain
    // across steps in-process instead (mirrors single-request headless).
    let scopes = RunScopes {
        session: BTreeMap::new(),
        cli: args.cli_vars.clone(),
        profile: profile_vars,
        workspace: workspace.manifest().vars.clone(),
    };
    // Proxy precedence mirrors `run`: CLI > workspace `churl.toml` > global config.
    let proxy = args
        .proxy
        .clone()
        .or_else(|| workspace.manifest().proxy.clone())
        .or_else(|| runtime.proxy.clone());
    let exec_opts = ExecuteOptions {
        max_body_bytes: runtime.max_body_bytes,
        redirect: runtime.redirect,
    };
    let root = workspace.root().to_path_buf();

    // --- stream: run each step, emitting its line as it completes ---
    let mut extracted: BTreeMap<String, String> = BTreeMap::new();
    let mut halted = false;
    let mut stats = StepStats::default();
    let mut asserts = AssertTotals::default();
    // The first hard step error's exit band wins over an assertion failure
    // (exit 1) — the single-request "transport errors still win" precedence.
    let mut hard_exit: Option<i32> = None;
    let mut any_assert_failed = false;
    // A broken extraction chain (a step's `extract` rule found nothing) is a run
    // failure too (exit 1): under the default `on_error: halt` it skips the
    // tail, and a downstream `{{captured}}` could never resolve — so a run that
    // silently exits 0 would be a CI footgun. Surfaced per-step in
    // `StepLine::extract_error`.
    let mut any_extract_failed = false;

    for step in ordered_steps(&sequence) {
        stats.total += 1;

        if halted {
            stats.skipped += 1;
            emit_step(
                json,
                &StepLine {
                    schema_version: SCHEMA_VERSION,
                    line_type: "step",
                    command: COMMAND,
                    seq: step.seq,
                    endpoint: &step.endpoint,
                    ok: false,
                    skipped: true,
                    data: None,
                    error: None,
                    extract_error: None,
                },
            );
            continue;
        }
        stats.ran += 1;

        // Resolve + load the step through the shared engine seam.
        let prepared = match prepare_step(&root, step, &extracted, &scopes) {
            Ok(prepared) => prepared,
            Err(err) => {
                let cli_err = map_prepare_error(err);
                hard_exit.get_or_insert(cli_err.kind.exit_code());
                stats.failed += 1;
                // A prepare failure is a step failure for `on_error` purposes.
                halted = should_halt(&StepResult::HttpError(String::new()), sequence.on_error);
                emit_step_error(json, step.seq, &step.endpoint, cli_err);
                continue;
            }
        };

        // One client per step, honouring that step's endpoint's durable
        // `insecure` flag (parity with `run`: `cli || config || endpoint`). No
        // cookie jar — chaining is via extracted values, not cookies (M8.4.1
        // scope; see docs/CLI.md).
        let client_cfg = ClientConfig {
            timeout: runtime.timeout,
            proxy: proxy.clone(),
            insecure: args.insecure || runtime.insecure || prepared.request.insecure,
            cookies: None,
        };
        let client = match build_client_with(&client_cfg) {
            Ok(client) => client,
            Err(err) => {
                let cli_err = crate::output::from_http_error(err);
                hard_exit.get_or_insert(cli_err.kind.exit_code());
                stats.failed += 1;
                halted = should_halt(&StepResult::HttpError(String::new()), sequence.on_error);
                emit_step_error(json, step.seq, &step.endpoint, cli_err);
                continue;
            }
        };

        let mut trace = args
            .verbose
            .then(|| DebugTrace::from_request(&prepared.request));
        let result = churl_core::http::execute_traced(
            &client,
            &prepared.request,
            &exec_opts,
            trace.as_mut(),
        )
        .await;

        // Merge chained values + decide halt through the SAME core seam the TUI
        // and `run_sequence` use, so flow control cannot drift. Extraction
        // rules feed the chain; assertions (below) are separate observations
        // that never themselves halt the run.
        let (step_result, step_extracted) = classify_step(&result, step);
        for (name, value) in &step_extracted {
            extracted.insert(name.clone(), value.clone());
        }
        if should_halt(&step_result, sequence.on_error) {
            halted = true;
        }

        match result {
            Ok(response) => {
                if let Some(trace) = trace.as_mut() {
                    trace.decisions.cookie_used = false;
                    trace.decisions.proxy = proxy.as_deref().map(churl_core::secrets::mask_url);
                }
                // A completed step is "failed" (for the summary's `steps.failed`
                // tally) when the core classifies it as an HTTP error status
                // (≥400) or a broken extraction — mirroring `StepResult` so the
                // count can't drift. An extraction failure additionally fails
                // the whole run (exit 1); an unasserted ≥400 status does not (it
                // is only visible in `data.response.status`, consistent with
                // single-request `run`).
                let extract_error = match &step_result {
                    StepResult::ExtractError(reason) => {
                        any_extract_failed = true;
                        stats.failed += 1;
                        Some(reason.clone())
                    }
                    StepResult::Failed { .. } => {
                        stats.failed += 1;
                        None
                    }
                    _ => None,
                };
                let data =
                    shape_exec_data(&prepared.request, &response, &prepared.assertions, trace);
                if let Some(report) = &data.assertions {
                    asserts.total += report.total;
                    asserts.passed += report.total - report.failed;
                    asserts.failed += report.failed;
                    if !report.passed {
                        any_assert_failed = true;
                    }
                }
                emit_step(
                    json,
                    &StepLine {
                        schema_version: SCHEMA_VERSION,
                        line_type: "step",
                        command: COMMAND,
                        seq: step.seq,
                        endpoint: &step.endpoint,
                        ok: true,
                        skipped: false,
                        data: Some(data),
                        error: None,
                        extract_error,
                    },
                );
            }
            Err(err) => {
                let cli_err = crate::output::from_http_error(err);
                hard_exit.get_or_insert(cli_err.kind.exit_code());
                stats.failed += 1;
                emit_step_error(json, step.seq, &step.endpoint, cli_err);
            }
        }
    }

    let overall_ok = hard_exit.is_none() && !any_assert_failed && !any_extract_failed;
    emit_summary(
        json,
        &SummaryLine {
            schema_version: SCHEMA_VERSION,
            line_type: "summary",
            command: COMMAND,
            ok: overall_ok,
            sequence: &args.name,
            steps: stats,
            assertions: asserts,
        },
    );

    match hard_exit {
        Some(code) => code,
        None if any_assert_failed || any_extract_failed => 1,
        None => 0,
    }
}

/// Maps a [`SequenceError`] (from [`prepare_step`]) onto a [`CliError`] — all
/// band 3 (resolution). An unresolved `{{var}}` reuses `unresolved-var` (the
/// same slug the single-request path emits); a traversal or load failure is
/// `sequence`-adjacent but semantically an unresolved *endpoint*, so it reuses
/// `endpoint-not-found`.
fn map_prepare_error(err: SequenceError) -> CliError {
    match err {
        SequenceError::Unresolved { names } => CliError::with_detail(
            ErrorKind::UnresolvedVar,
            format!(
                "unresolved variable(s), no scope (nor process env) resolved: {}",
                names.join(", ")
            ),
            serde_json::json!({ "vars": names }),
        ),
        other => CliError::new(ErrorKind::EndpointNotFound, other.to_string()),
    }
}

/// Emits a hard-error step line (`data:null`, `error` populated).
fn emit_step_error(json: bool, seq: u32, endpoint: &str, err: CliError) {
    emit_step(
        json,
        &StepLine {
            schema_version: SCHEMA_VERSION,
            line_type: "step",
            command: COMMAND,
            seq,
            endpoint,
            ok: false,
            skipped: false,
            data: None,
            error: Some(EnvelopeError {
                kind: err.kind,
                message: err.message,
                detail: err.detail,
            }),
            extract_error: None,
        },
    );
}

/// Emits one step line: a compact NDJSON object on stdout under `--json`
/// (flushed for live streaming), else a human checklist row on stderr.
fn emit_step(json: bool, line: &StepLine) {
    if json {
        print_json(line);
        return;
    }
    if line.skipped {
        eprintln!(
            "  ⊘ [{}] {} — skipped (an earlier step halted the run)",
            line.seq, line.endpoint
        );
    } else if let Some(err) = &line.error {
        eprintln!("  ✗ [{}] {} — {}", line.seq, line.endpoint, err.message);
    } else if let Some(data) = &line.data {
        let status = data.response.status;
        if let Some(reason) = &line.extract_error {
            eprintln!(
                "  ✗ [{}] {} → {} — extraction failed: {}",
                line.seq, line.endpoint, status, reason
            );
            return;
        }
        match &data.assertions {
            Some(report) if report.failed > 0 => eprintln!(
                "  ✗ [{}] {} → {} ({} passed, {} failed)",
                line.seq,
                line.endpoint,
                status,
                report.total - report.failed,
                report.failed
            ),
            Some(report) => eprintln!(
                "  ✓ [{}] {} → {} ({} assertions)",
                line.seq, line.endpoint, status, report.total
            ),
            None => eprintln!("  ✓ [{}] {} → {}", line.seq, line.endpoint, status),
        }
    }
}

/// Emits the terminal summary line: a compact NDJSON object on stdout under
/// `--json`, else a `PASS`/`FAIL` rollup on stderr.
fn emit_summary(json: bool, line: &SummaryLine) {
    if json {
        print_json(line);
        return;
    }
    let verdict = if line.ok { "PASS" } else { "FAIL" };
    eprintln!(
        "{verdict} {}: {}/{} steps ran, {} skipped, {} errored; {}/{} assertions passed",
        line.sequence,
        line.steps.ran,
        line.steps.total,
        line.steps.skipped,
        line.steps.failed,
        line.assertions.passed,
        line.assertions.total,
    );
}

/// Prints one compact JSON object followed by a newline to stdout and flushes,
/// so a downstream consumer sees each step as it completes (stdout is
/// block-buffered when piped — the whole point of an NDJSON *stream*).
fn print_json<T: Serialize>(line: &T) {
    match serde_json::to_string(line) {
        Ok(text) => {
            let mut stdout = std::io::stdout();
            // A broken pipe (consumer went away) is not our error to report.
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        }
        Err(err) => eprintln!("error: failed to serialize NDJSON line: {err}"),
    }
}
