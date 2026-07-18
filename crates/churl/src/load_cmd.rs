//! `churl load <endpoint>` — a headless load run plus assertions on the
//! **aggregate** stats. Resolves a saved endpoint from the cwd workspace
//! exactly like [`crate::run_cmd`], fires N concurrent copies of the resolved
//! request through the wiremock-tested `churl_core::load::run_load` engine (the
//! same engine the TUI load runner drives), then reports counts + latency
//! percentiles + throughput and gates the run on `stats.*` assertions.
//!
//! # Output contract (freeze-once — `docs/CLI.md`, "Load runs")
//!
//! A load run is **one aggregate result**, so — unlike `run-seq`'s per-step
//! NDJSON — it emits a **single** `{schema_version, ok, command, data, error}`
//! envelope via [`crate::output::emit`], reusing the exact `run`/`send` seam.
//! `data` carries `config` (the effective `total`/`concurrency`/`gap_ms`),
//! `stats` (the frozen aggregate block — counts, rates in `0..1`, latencies in
//! milliseconds, `rps`), and `assertions` (an [`AssertionReport`] or `null`).
//! A failed `stats.*` assertion exits **1** with a success-shaped envelope —
//! the same documented exception to "`ok` mirrors the exit code" as `run`
//! (see [`crate::output::SuccessExitCode`]).
//!
//! Human (non-`--json`) mode prints a readable stats summary + an assertion
//! checklist to **stderr** (stdout stays empty — a load run has no single body
//! to emit), mirroring the single-request checklist.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use churl_core::assert::{
    AssertionReport, StatAssertion, parse_stats_assertion, run_stats_assertions,
};
use churl_core::http::{ClientConfig, ExecuteOptions, build_client_with};
use churl_core::load::{LoadConfig, LoadStats, run_load, stats};
use churl_core::template::{Resolver, Scope};

use crate::RuntimeCfg;
use crate::output::{CliError, ErrorKind, SuccessExitCode};
use crate::resolve::{resolve_endpoint, resolve_profile_vars};

/// CLI-sourced inputs to `load` (everything that can vary per invocation; the
/// process-wide [`RuntimeCfg`] carries the global-config-sourced knobs).
pub struct LoadArgs {
    pub endpoint_path: String,
    /// `--total N`: total request copies to fire (default: [`LoadConfig::default`]).
    pub total: Option<usize>,
    /// `--concurrency C`: max requests in flight at once (default: as above).
    pub concurrency: Option<usize>,
    /// `--gap MS`: minimum delay between launches, mapped to `LoadConfig.interval`.
    pub gap_ms: Option<u64>,
    pub cli_vars: BTreeMap<String, String>,
    pub profile: Option<String>,
    pub proxy: Option<String>,
    pub insecure: bool,
    /// Raw `--assert 'stats.<field> <op> <value>'` flag strings.
    pub cli_asserts: Vec<String>,
}

/// The effective run config echoed into the envelope (`data.config`).
#[derive(Debug, Serialize)]
struct LoadConfigOut {
    total: usize,
    concurrency: usize,
    gap_ms: u64,
}

/// The frozen aggregate stats block (`data.stats`). Latencies are **integer
/// milliseconds** and `null` when no request completed; rates are `0..1` floats
/// and `null` only when zero requests were attempted; `rps` is a float and
/// `null` when nothing was attempted or the run took no measurable time; counts
/// are integers. Keys are always present (a `null` value, never omitted) so the
/// shape is stable for a machine consumer.
#[derive(Debug, Serialize)]
struct StatsOut {
    /// Requests attempted (`ok + failed + errored`).
    count: usize,
    ok: usize,
    failed: usize,
    errored: usize,
    success_rate: Option<f64>,
    error_rate: Option<f64>,
    min_ms: Option<u128>,
    p50_ms: Option<u128>,
    p95_ms: Option<u128>,
    max_ms: Option<u128>,
    mean_ms: Option<u128>,
    rps: Option<f64>,
}

/// The full `data` payload of a `load` envelope.
#[derive(Debug, Serialize)]
pub struct LoadData {
    config: LoadConfigOut,
    stats: StatsOut,
    /// `null` when no `--assert` was given (mirrors the `run`/`send`
    /// assertion-free contract); the populated [`AssertionReport`] otherwise.
    assertions: Option<AssertionReport>,
}

impl SuccessExitCode for LoadData {
    /// Exit 1 iff `stats.*` assertions were given and at least one failed; 0
    /// otherwise — the load run itself completed (an all-errored batch is still
    /// a successful *command*, its failures visible in `stats.errored`), so the
    /// envelope stays success-shaped, the same exception `run`/`send` document.
    fn success_exit_code(&self) -> i32 {
        match &self.assertions {
            Some(report) if !report.passed => 1,
            _ => 0,
        }
    }
}

/// Opens the cwd workspace, resolves `args.endpoint_path` + its collection var
/// chain (exactly like `run`), parses the `stats.*` assertions pre-flight,
/// substitutes and refuses an unresolved `{{var}}`, builds one client, and
/// fires the load run — measuring wall-clock around it for `rps`.
pub async fn run(args: LoadArgs, cwd: &Path, runtime: &RuntimeCfg) -> Result<LoadData, CliError> {
    let workspace = churl::tui::app::open_workspace(cwd)
        .map_err(|err| CliError::new(ErrorKind::NoWorkspace, err.to_string()))?
        .ok_or_else(|| {
            CliError::new(
                ErrorKind::NoWorkspace,
                format!(
                    "no churl workspace at {} (no churl.toml) — run `churl init` first",
                    cwd.display()
                ),
            )
        })?;

    let resolved = resolve_endpoint(&workspace, &args.endpoint_path)?;
    let profile_vars = resolve_profile_vars(Some(&workspace), args.profile.as_deref())?;

    // Parse + validate every `--assert` BEFORE firing a request: a bad target
    // or grammar is a pre-flight usage error (band 5, `invalid-assertion`),
    // never something to discover after a load run already hit the network.
    let assertions = parse_stats_assertions(&args.cli_asserts)?;

    // Effective run config — read from Copy flag fields before any of `args`'s
    // owned fields (`cli_vars`, `proxy`) are moved below.
    let cfg = load_config(&args);

    // Resolver scopes match `App::build_resolver`/`run_cmd`: cli → profile →
    // collection ancestor chain (leaf → root). No "session" scope — a one-shot
    // process never captures one.
    let mut scopes = vec![
        Scope::new("cli", args.cli_vars),
        Scope::new("profile", profile_vars),
    ];
    for vars in resolved.ancestor_vars {
        scopes.push(Scope::new("collection", vars));
    }

    // Substitute once — the single resolved request is cloned per copy inside
    // `run_load` (no re-resolution per copy) — and refuse an unresolved
    // placeholder (fail loud, parity with `run`).
    let mut request = resolved.endpoint.request;
    Resolver::new(scopes).substitute_request(&mut request);
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

    // Proxy precedence: CLI > workspace `churl.toml` > global config; effective
    // insecure: CLI `-k` or config default, or the endpoint's own durable flag
    // — both mirror `run`.
    let effective_proxy = args
        .proxy
        .or_else(|| workspace.manifest().proxy.clone())
        .or_else(|| runtime.proxy.clone());
    let effective_insecure = args.insecure || runtime.insecure || request.insecure;
    let client = build_client_with(&ClientConfig {
        timeout: runtime.timeout,
        proxy: effective_proxy,
        insecure: effective_insecure,
        cookies: None,
    })
    .map_err(crate::output::from_http_error)?;

    let exec_opts = ExecuteOptions {
        max_body_bytes: runtime.max_body_bytes,
        redirect: runtime.redirect,
    };

    // `run_load` returns no elapsed (it uses `start.elapsed()` only for
    // internal pacing), so the wall-clock for `rps` is measured HERE around the
    // call — keeping the core signature un-widened (see docs/DECISIONS.md).
    let started = Instant::now();
    let outcomes = run_load(&client, &request, &cfg, &exec_opts, None).await;
    let elapsed = started.elapsed();

    let run_stats = stats(&outcomes);
    let rps = throughput(run_stats.attempted(), elapsed);
    // `None` when no `--assert` was given — keeps `data.assertions` `null`, the
    // same back-compat shape `run`/`send` use for an assertion-free call.
    let report =
        (!assertions.is_empty()).then(|| run_stats_assertions(&assertions, &run_stats, rps));

    Ok(LoadData {
        config: LoadConfigOut {
            total: cfg.total,
            concurrency: cfg.concurrency,
            gap_ms: u64::try_from(cfg.interval.as_millis()).unwrap_or(u64::MAX),
        },
        stats: stats_out(&run_stats, rps),
        assertions: report,
    })
}

/// Builds the [`LoadConfig`] from the CLI flags over the engine defaults
/// (`--gap MS` → `interval`). Absent flags keep [`LoadConfig::default`].
fn load_config(args: &LoadArgs) -> LoadConfig {
    let defaults = LoadConfig::default();
    LoadConfig {
        total: args.total.unwrap_or(defaults.total),
        concurrency: args.concurrency.unwrap_or(defaults.concurrency),
        interval: args
            .gap_ms
            .map(Duration::from_millis)
            .unwrap_or(defaults.interval),
    }
}

/// Requests attempted per second over the run's measured wall-clock, or `None`
/// when nothing was attempted or the run took no measurable time (guards a
/// divide-by-zero into an `inf`). Uses **attempted** (every fired copy), not
/// completed, so `rps` reflects the offered load rate — documented in
/// `docs/CLI.md` and `docs/DECISIONS.md`.
fn throughput(attempted: usize, elapsed: Duration) -> Option<f64> {
    let secs = elapsed.as_secs_f64();
    (attempted > 0 && secs > 0.0).then(|| attempted as f64 / secs)
}

/// Projects [`LoadStats`] (+ the caller-measured `rps`) into the frozen
/// envelope block. Latencies use `as_millis` — the same rounding
/// [`churl_core::load::StatTarget::resolve`] applies, so a `*_ms` field equals
/// the value a `stats.*` assertion saw. Rates call the [`LoadStats`] methods
/// directly (the single source of truth); `rps` is the *same* value handed to
/// the assertions, so the reported and asserted throughput cannot disagree.
fn stats_out(s: &LoadStats, rps: Option<f64>) -> StatsOut {
    let ms = |d: Option<Duration>| d.map(|d| d.as_millis());
    StatsOut {
        count: s.attempted(),
        ok: s.ok,
        failed: s.failed,
        errored: s.errored,
        success_rate: s.success_rate(),
        error_rate: s.error_rate(),
        min_ms: ms(s.min),
        p50_ms: ms(s.median),
        p95_ms: ms(s.p95),
        max_ms: ms(s.max),
        mean_ms: ms(s.mean),
        rps,
    }
}

/// Parses every `--assert` via [`parse_stats_assertion`], mapping the first
/// failure onto [`ErrorKind::InvalidAssertion`] (band 5) — a bad `stats.*`
/// target or grammar is a usage/input mistake, never a runtime request failure.
fn parse_stats_assertions(exprs: &[String]) -> Result<Vec<StatAssertion>, CliError> {
    exprs
        .iter()
        .map(|expr| {
            parse_stats_assertion(expr).map_err(|err| {
                CliError::with_detail(
                    ErrorKind::InvalidAssertion,
                    format!("invalid --assert {expr:?}: {err}"),
                    serde_json::json!({ "assert": expr }),
                )
            })
        })
        .collect()
}

/// Human-mode rendering of a successful [`LoadData`]: a readable stats summary
/// plus the assertion checklist, all on **stderr** (stdout stays empty — a load
/// run has no single body). Mirrors `run`'s human checklist.
pub fn print_human(data: &LoadData) {
    let c = &data.config;
    let s = &data.stats;
    eprintln!(
        "load run: total={} concurrency={} gap={}ms",
        c.total, c.concurrency, c.gap_ms
    );
    eprintln!(
        "  {} attempted · {} ok · {} failed · {} errored",
        s.count, s.ok, s.failed, s.errored
    );
    if let (Some(success), Some(error)) = (s.success_rate, s.error_rate) {
        eprintln!(
            "  success {:.1}% · error {:.1}%",
            success * 100.0,
            error * 100.0
        );
    }
    // Latency fields are `Some` together (they derive from the same completed
    // timings), so one guard covers them; skipped entirely when nothing
    // completed.
    if let (Some(min), Some(p50), Some(p95), Some(max), Some(mean)) =
        (s.min_ms, s.p50_ms, s.p95_ms, s.max_ms, s.mean_ms)
    {
        eprintln!("  latency min/p50/p95/max {min}/{p50}/{p95}/{max}ms · mean {mean}ms");
    }
    if let Some(rps) = s.rps {
        eprintln!("  {rps:.1} req/s");
    }
    if let Some(report) = &data.assertions {
        crate::headless::print_assertion_checklist(report);
    }
}
