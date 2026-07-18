//! `churl run <endpoint>` — headless execution of a saved endpoint, resolved
//! from the cwd workspace by its `collection/.../endpoint name` display path
//! (see [`crate::resolve`]) and driven through the exact same
//! `churl_core::http::execute` seam the TUI's `send_request` uses.

use std::collections::BTreeMap;
use std::path::Path;

use churl_core::http::{ClientConfig, ExecuteOptions};
use churl_core::template::Scope;

use crate::RuntimeCfg;
use crate::headless::{ExecData, ExecInputs, run_execution};
use crate::output::{CliError, ErrorKind};
use crate::resolve::{resolve_endpoint, resolve_profile_vars};

/// CLI-sourced inputs to `run` (everything that can vary per invocation; the
/// process-wide [`RuntimeCfg`] carries the global-config-sourced knobs).
pub struct RunArgs {
    pub endpoint_path: String,
    pub cli_vars: BTreeMap<String, String>,
    pub profile: Option<String>,
    pub proxy: Option<String>,
    pub insecure: bool,
    /// `-v`/`--verbose`: prints the resolved endpoint file path to stderr
    /// before executing (human mode only — `headless::print_human` covers
    /// the rest of the request/response trace); ALSO gates capturing a
    /// `DebugTrace` into `data.trace` under `--json` (see
    /// [`run_execution`]'s `capture` flag) — the sole capture gate, M8.3.
    pub verbose: bool,
    /// Raw `--assert` flag strings, parsed and appended after the resolved
    /// endpoint's own persisted `[[assertions]]`.
    pub cli_asserts: Vec<String>,
}

/// Opens the cwd workspace, resolves `args.endpoint_path`, builds the
/// resolver scopes in the TUI's exact precedence order (cli → profile →
/// collection ancestor chain leaf → root; no "session" scope — a one-shot
/// process never captures one), and executes.
pub async fn run(args: RunArgs, cwd: &Path, runtime: &RuntimeCfg) -> Result<ExecData, CliError> {
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
    if args.verbose {
        eprintln!(
            "resolved {} -> {}",
            args.endpoint_path,
            resolved.file.display()
        );
    }
    let profile_vars = resolve_profile_vars(Some(&workspace), args.profile.as_deref())?;

    // Precedence order matches `App::build_resolver`: cli → profile →
    // collection ancestor chain, innermost (leaf) → outermost (root).
    let mut scopes = vec![
        Scope::new("cli", args.cli_vars),
        Scope::new("profile", profile_vars),
    ];
    for vars in resolved.ancestor_vars {
        scopes.push(Scope::new("collection", vars));
    }

    // Proxy precedence: CLI > workspace `churl.toml` > global config (mirrors
    // `tui::app::pure::resolve_proxy`; reimplemented here rather than widening
    // that `pub(super)` helper's visibility for a one-line rule).
    let effective_proxy = args
        .proxy
        .or_else(|| workspace.manifest().proxy.clone())
        .or_else(|| runtime.proxy.clone());
    // Effective insecure: CLI `-k` or the config default forces it on for the
    // whole invocation; the endpoint's own durable `insecure` flag can also
    // turn it on for just this request (mirrors `App::client_for`'s
    // `request.insecure || session_insecure`).
    let effective_insecure =
        args.insecure || runtime.insecure || resolved.endpoint.request.insecure;

    let inputs = ExecInputs {
        client_cfg: ClientConfig {
            timeout: runtime.timeout,
            proxy: effective_proxy,
            insecure: effective_insecure,
            cookies: None,
        },
        exec_opts: ExecuteOptions {
            max_body_bytes: runtime.max_body_bytes,
            redirect: runtime.redirect,
        },
    };

    // Effective set: the endpoint's own persisted assertions, THEN the CLI
    // `--assert` flags (append) — see `docs/CLI.md`, "Assertions".
    let mut assertions = resolved.endpoint.assertions;
    assertions.extend(crate::headless::parse_cli_assertions(&args.cli_asserts)?);

    run_execution(
        resolved.endpoint.request,
        scopes,
        inputs,
        &assertions,
        args.verbose,
    )
    .await
}
