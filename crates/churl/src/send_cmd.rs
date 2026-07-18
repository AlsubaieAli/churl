//! `churl send` — an ad-hoc one-shot request from inline flags. No saved
//! endpoint, no workspace required: when the cwd happens to be an open
//! workspace its root `[vars]` and named profiles are still available (so a
//! script can do `churl send --profile prod {{base_url}}/health`), but their
//! absence is not an error.

use std::collections::BTreeMap;
use std::path::Path;

use churl_core::http::{ClientConfig, ExecuteOptions};
use churl_core::model::{Body, Header, Method, Request};
use churl_core::template::Scope;

use crate::RuntimeCfg;
use crate::headless::{ExecData, ExecInputs, run_execution};
use crate::output::{CliError, ErrorKind};
use crate::resolve::resolve_profile_vars;

/// CLI-sourced inputs to `send`.
pub struct SendArgs {
    pub url: String,
    /// `None` means "no explicit `-X`/`--method`" — curl semantics apply (a
    /// body implies POST, else GET), matching `churl import`'s own
    /// `ImportResult` method derivation.
    pub method: Option<Method>,
    /// Already-validated `NAME:VALUE` pairs (clap's value parser rejects a
    /// malformed `-H`/`--header` at the usage layer — exit 2 — before this
    /// ever runs).
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub cli_vars: BTreeMap<String, String>,
    pub profile: Option<String>,
    pub proxy: Option<String>,
    pub insecure: bool,
    /// `-v`/`--verbose`: gates BOTH the stderr request/response trace
    /// (`headless::print_human`) AND, new in M8.3, a `DebugTrace` captured
    /// into `data.trace` under `--json` (see [`run_execution`]'s `capture`
    /// flag). `send` has no config-level `debug` toggle of its own — this is
    /// the sole capture gate.
    pub verbose: bool,
    /// Raw `--assert` flag strings — `send` has no persisted endpoint, so
    /// these are the whole assertion set.
    pub cli_asserts: Vec<String>,
}

pub async fn run(args: SendArgs, cwd: &Path, runtime: &RuntimeCfg) -> Result<ExecData, CliError> {
    // A workspace at cwd is optional for `send` — a missing one is not an
    // error, but an I/O failure reading an existing-but-broken one still is.
    let workspace = churl::tui::app::open_workspace(cwd)
        .map_err(|err| CliError::new(ErrorKind::NoWorkspace, err.to_string()))?;

    let profile_vars = resolve_profile_vars(workspace.as_ref(), args.profile.as_deref())?;
    let root_vars = workspace
        .as_ref()
        .map(|w| w.manifest().vars.clone())
        .unwrap_or_default();

    // Same precedence shape as `run`/`App::build_resolver`, minus the
    // collection ancestor chain (there is no saved endpoint, hence no
    // collection to walk) — the root collection's `[vars]` stands in as the
    // sole "collection" scope, mirroring `App::build_env_resolver`.
    let scopes = vec![
        Scope::new("cli", args.cli_vars),
        Scope::new("profile", profile_vars),
        Scope::new("collection", root_vars),
    ];

    let headers: Vec<Header> = args
        .headers
        .into_iter()
        .map(|(name, value)| Header {
            name,
            value,
            enabled: true,
        })
        .collect();
    let method = args.method.unwrap_or(if args.body.is_some() {
        Method::Post
    } else {
        Method::Get
    });
    let body = args.body.map(|content| {
        let kind = churl_core::import::derive_body_kind(&content, &headers);
        Body { kind, content }
    });

    let request = Request {
        method,
        url: args.url,
        headers,
        params: Vec::new(),
        body,
        auth: None,
        insecure: false,
    };

    let effective_proxy = args
        .proxy
        .or_else(|| workspace.as_ref().and_then(|w| w.manifest().proxy.clone()))
        .or_else(|| runtime.proxy.clone());
    let effective_insecure = args.insecure || runtime.insecure;

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

    let assertions = crate::headless::parse_cli_assertions(&args.cli_asserts)?;
    run_execution(request, scopes, inputs, &assertions, args.verbose).await
}

/// Parses a `-H`/`--header` value as `NAME:VALUE` (curl shape), trimming
/// whitespace around both. Used as a clap `value_parser` so a malformed
/// header is a usage error (exit 2), never a runtime [`CliError`].
pub fn parse_header(s: &str) -> Result<(String, String), String> {
    match s.split_once(':') {
        Some((name, value)) if !name.trim().is_empty() => {
            Ok((name.trim().to_owned(), value.trim().to_owned()))
        }
        _ => Err(format!("expected NAME:VALUE, got {s:?}")),
    }
}

/// Parses a `-X`/`--method` value against churl's closed [`Method`] set.
/// Used as a clap `value_parser` so an unsupported method (including curl's
/// own free-form `-X`, which accepts any string) is a usage error (exit 2),
/// never a runtime [`CliError`] — a deliberate M8.2 scope cut, see the build
/// report.
pub fn parse_method(s: &str) -> Result<Method, String> {
    s.parse().map_err(|_| {
        format!(
            "unknown HTTP method {s:?} (expected one of: GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS)"
        )
    })
}
