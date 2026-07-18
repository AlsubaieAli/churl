use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;
use serde::Serialize;

use output::{CliError, ErrorKind};

mod headless;
mod init;
mod load_cmd;
mod output;
mod resolve;
mod run_cmd;
mod send_cmd;
mod seq_cmd;
mod uninstall;
mod update;

#[derive(Debug, Parser)]
#[command(name = "churl", about = "Terminal HTTP client", version)]
struct Cli {
    /// Set a template variable (repeatable): `--var key=value`. Highest-precedence
    /// scope in the `{{var}}` resolver, above profiles and workspace vars. A
    /// missing `=` is a usage error (parsed by clap → exit 2).
    #[arg(long = "var", value_name = "KEY=VALUE", global = true, value_parser = parse_var)]
    vars: Vec<(String, String)>,
    /// Activate a named profile at startup (unknown name is an error).
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Import a Postman v2.1 collection JSON file into the cwd workspace before
    /// launching the TUI (endpoints land as normal file-per-endpoint TOML).
    #[arg(long = "import-collection", value_name = "FILE", global = true)]
    import_collection: Option<PathBuf>,
    /// Route every request through this HTTP/HTTPS proxy for the session (highest
    /// precedence, above `churl.toml`/config/env). A `user:pass@` proxy is
    /// accepted here but never persisted. Also settable live in the Options overlay.
    #[arg(long = "proxy", value_name = "URL", global = true)]
    proxy: Option<String>,
    /// Disable TLS certificate verification for the session (accepts self-signed
    /// and hostname-mismatched certs). Session-scoped; also toggled with
    /// `<leader>k`. Use with care.
    #[arg(short = 'k', long = "insecure", global = true)]
    insecure: bool,
    /// Emit machine-readable JSON on stdout instead of human-readable output —
    /// for `run`/`send`/`import`/`load` (a single envelope) and `run-seq` (an
    /// NDJSON stream); see docs/CLI.md for the frozen schema. Disables color,
    /// spinners, prompts, and bracketed paste. Every other subcommand ignores
    /// this flag.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Import a curl command as an endpoint into the cwd workspace (`--stdout`
    /// prints TOML instead; `--out FILE` writes an arbitrary file).
    ///
    /// Three input forms: paste the whole command as ONE quoted string
    /// (`churl import "curl 'url' -H '…'"`); paste it RAW so the shell tokenises it
    /// (`churl import curl 'url' -H '…'` — `-H` etc. are captured, not parsed as
    /// churl flags); or omit it / pass `-` to read from stdin
    /// (`pbpaste | churl import`). Put `--name`/`--stdout`/`--out` BEFORE the command.
    Import {
        /// Override the endpoint name derived from the URL (place before the curl)
        #[arg(long)]
        name: Option<String>,
        /// Print the endpoint TOML to stdout instead of writing it into the cwd
        /// workspace (the pre-M8.2 default; place before the curl)
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
        /// Write the endpoint TOML to this file instead of the cwd workspace
        /// (place before the curl)
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// The curl command: one quoted string, raw trailing tokens, or empty/`-`
        /// to read from stdin.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        curl: Vec<String>,
    },
    /// Execute a saved endpoint headlessly — no TUI. Resolves `endpoint` from
    /// the cwd workspace, applies `--var`/`--profile`, and prints the response
    /// (body on stdout; `--json` for the full structured envelope).
    Run {
        /// Endpoint path: `collection/sub/endpoint name` (root-level: just the
        /// endpoint's name). Quote it when the name has spaces.
        endpoint: String,
        /// Print a request/response debug trace to stderr in human mode; under
        /// `--json`, additionally adds a `data.trace` object to the envelope
        /// (resolved request, var-resolution steps, redirect hops, auth/
        /// cookie/proxy decisions — secrets masked throughout). See
        /// `docs/CLI.md`, "Debug trace (`-v`)".
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Assert a response value (repeatable): `"<target> <op> <value>"`,
        /// e.g. `--assert 'status == 200'`, `--assert '$.data.id exists'`.
        /// Runs AFTER the endpoint's own persisted `[[assertions]]` (append).
        /// A failing assertion set exits 1 even though the request itself
        /// succeeded — see `docs/CLI.md`, "Assertions".
        #[arg(long = "assert", value_name = "EXPR")]
        assert: Vec<String>,
    },
    /// Run a saved request sequence headlessly — no TUI. Resolves
    /// `sequences/<name>.toml` in the cwd workspace, runs each step end-to-end
    /// (values extracted from one step feed later ones), and gates each step on
    /// its endpoint's persisted `[[assertions]]`. Under `--json` it streams
    /// NDJSON: one object per step plus a terminal summary line. Any failed
    /// assertion exits 1; a transport/resolution error wins with its own band
    /// (3/4/5). See `docs/CLI.md`, "Sequence & load runs".
    #[command(name = "run-seq")]
    RunSeq {
        /// Sequence file stem: `run-seq checkout` runs `sequences/checkout.toml`.
        name: String,
        /// Add a `data.trace` object to each step's envelope under `--json`
        /// (resolved request, redirect hops, auth/cookie/proxy decisions —
        /// secrets masked); prints a per-step trace to stderr in human mode.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Run a headless load test against a saved endpoint — no TUI. Resolves
    /// `endpoint` from the cwd workspace exactly like `run`, fires N concurrent
    /// copies, and reports aggregate stats (counts, latency percentiles,
    /// throughput). `--assert 'stats.<field> <op> <value>'` gates the run on
    /// the aggregate (e.g. `stats.p95 < 500`, `stats.error_rate <= 0.01`); a
    /// failing assertion exits 1. Under `--json` the run is a single aggregate
    /// envelope. See `docs/CLI.md`, "Load runs (`load`)".
    Load {
        /// Endpoint path: `collection/sub/endpoint name` (root-level: just the
        /// endpoint's name). Quote it when the name has spaces.
        endpoint: String,
        /// Total number of request copies to fire (default: 10).
        #[arg(long)]
        total: Option<usize>,
        /// Maximum number of requests in flight at once (default: 5).
        #[arg(long)]
        concurrency: Option<usize>,
        /// Minimum delay between successive launches, milliseconds (default: 0
        /// = burst as fast as concurrency permits).
        #[arg(long = "gap", value_name = "MS")]
        gap: Option<u64>,
        /// Assert on an aggregate stat (repeatable):
        /// `"stats.<field> <op> <value>"`, e.g. `--assert 'stats.p95 < 500'`,
        /// `--assert 'stats.error_rate <= 0.01'`. A failing assertion exits 1
        /// even though the run itself completed — see `docs/CLI.md`,
        /// "Load runs".
        #[arg(long = "assert", value_name = "EXPR")]
        assert: Vec<String>,
    },
    /// Send an ad-hoc one-shot request — no saved endpoint, no workspace
    /// required. Accepts curl-mnemonic flags (`-X`/`-H`/`-d`/`--url`) and
    /// churl-native aliases (`--method`/`--header`/`--body`); `--var` still
    /// applies (and `--profile`/workspace vars, when the cwd happens to be one).
    Send {
        /// Target URL (or use `--url`)
        #[arg(
            value_name = "URL",
            conflicts_with = "url",
            required_unless_present = "url"
        )]
        url_pos: Option<String>,
        /// Target URL (alternative to the positional form)
        #[arg(long = "url", value_name = "URL")]
        url: Option<String>,
        /// HTTP method; curl `-X`, churl `--method` (default: GET, or POST when
        /// `--body`/`-d` is given and no explicit method — curl semantics)
        #[arg(short = 'X', long = "method", value_name = "METHOD", value_parser = send_cmd::parse_method)]
        method: Option<churl_core::model::Method>,
        /// Request header `Name: Value`; curl `-H`, churl `--header` (repeatable)
        #[arg(short = 'H', long = "header", value_name = "NAME:VALUE", value_parser = send_cmd::parse_header)]
        header: Vec<(String, String)>,
        /// Request body; curl `-d`, churl `--body`
        #[arg(short = 'd', long = "body", value_name = "BODY")]
        body: Option<String>,
        /// Print a request/response debug trace to stderr in human mode; under
        /// `--json`, additionally adds a `data.trace` object to the envelope
        /// (resolved request, var-resolution steps, redirect hops, auth/
        /// cookie/proxy decisions — secrets masked throughout). See
        /// `docs/CLI.md`, "Debug trace (`-v`)".
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Assert a response value (repeatable): `"<target> <op> <value>"`,
        /// e.g. `--assert 'status == 200'`, `--assert 'header:Content-Type
        /// contains json'`. `send` has no persisted endpoint, so these are
        /// the whole assertion set. A failing set exits 1 even though the
        /// request itself succeeded — see `docs/CLI.md`, "Assertions".
        #[arg(long = "assert", value_name = "EXPR")]
        assert: Vec<String>,
    },
    /// Print the effective keymap (every action, its bindings, and default/overridden)
    Keymaps,
    /// Inspect or clear the current workspace's persistent cookie jar (headless)
    Cookies {
        #[command(subcommand)]
        action: CookiesAction,
    },
    /// Scaffold a churl workspace: a blank `churl.toml` by default, or a demo
    /// workspace with example endpoints via `--demo`.
    Init {
        /// Directory to scaffold (default: the current directory)
        path: Option<PathBuf>,
        /// Scaffold the demo workspace (example endpoints, a profile, and
        /// template variables targeting httpbingo.org) instead of a blank one
        #[arg(long)]
        demo: bool,
    },
    /// Print a shell completion script for `churl` to stdout
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print a roff(7) man page for `churl` to stdout
    Man,
    /// Update churl in place to the latest GitHub release (verified, reversible)
    Update {
        /// Report the available version and exit without downloading or replacing
        #[arg(long)]
        check: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Remove the churl binary; `--purge` also removes churl's config and state
    Uninstall {
        /// Also delete churl's config directory and local state database
        #[arg(long)]
        purge: bool,
        /// Skip the confirmation prompt (only relevant with `--purge`)
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

/// `churl cookies …`: headless inspection/clearing of the persistent jar.
#[derive(Debug, Subcommand)]
enum CookiesAction {
    /// Print the stored cookies for the current workspace (domain · name · value)
    List,
    /// Clear the stored cookies for the current workspace
    Clear,
}

/// clap value-parser for `--var key=value`. A missing `=` is a genuine clap
/// usage error (exit 2, clap-owned — envelope-exempt per the contract's band-2
/// carve-out), so a malformed `--var` never bubbles out of `main` as an
/// exit-1 anyhow error under `--json`.
fn parse_var(pair: &str) -> std::result::Result<(String, String), String> {
    match pair.split_once('=') {
        Some((key, value)) => Ok((key.to_owned(), value.to_owned())),
        None => Err(format!("expected key=value, got {pair:?}")),
    }
}

/// Collects the clap-parsed `--var` pairs into a map (last write wins, matching
/// the old `parse_vars` insertion order).
fn vars_map(pairs: &[(String, String)]) -> BTreeMap<String, String> {
    pairs.iter().cloned().collect()
}

/// Process-wide execution knobs sourced from the global config file only
/// (never per-invocation) — resolved once per `run`/`send` invocation, so
/// neither headless command needs its own fallible config-parsing path.
struct RuntimeCfg {
    timeout: std::time::Duration,
    proxy: Option<String>,
    insecure: bool,
    max_body_bytes: u64,
    redirect: churl_core::config::RedirectPolicy,
    /// The `[load]` guardrail caps — used by `churl load` to refuse a headless
    /// run above the hard ceiling (`run`/`send` ignore it).
    load_caps: churl_core::load::LoadCaps,
}

/// Resolves [`RuntimeCfg`] from the global config, mapping any failure to a
/// band-3 [`ErrorKind::ConfigError`] `CliError` so it flows through
/// [`output::emit`] as a proper envelope (never an exit-1 anyhow bubble that
/// would collide with the RESERVED assertion-failure code and print no
/// envelope under `--json`). A corrupt config is a pre-flight *resolution*
/// failure, not a usage error — hence band 3, not band 2.
fn build_runtime_cfg() -> std::result::Result<RuntimeCfg, CliError> {
    let config = churl_core::config::load_global_config()
        .map_err(|err| CliError::new(ErrorKind::ConfigError, err.to_string()))?;
    let redirect = config
        .redirect()
        .map_err(|err| CliError::new(ErrorKind::ConfigError, err.to_string()))?;
    Ok(RuntimeCfg {
        timeout: config.timeout(),
        proxy: config.proxy().map(str::to_owned),
        insecure: config.insecure(),
        max_body_bytes: config.max_body_bytes(),
        redirect,
        load_caps: config.load_caps(),
    })
}

/// The current working directory, mapped to a band-3 [`ErrorKind::ConfigError`]
/// `CliError` on failure so a `run`/`send` invocation in an unreadable/deleted
/// cwd surfaces an envelope rather than an exit-1 bubble.
fn headless_cwd() -> std::result::Result<PathBuf, CliError> {
    std::env::current_dir().map_err(|err| {
        CliError::new(
            ErrorKind::ConfigError,
            format!("cannot determine current directory: {err}"),
        )
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        Some(Command::Import {
            name,
            stdout,
            out,
            curl,
        }) => {
            std::process::exit(run_import(curl, name, stdout, out, json));
        }
        Some(Command::Run {
            endpoint,
            verbose,
            assert,
        }) => {
            // Every fallible pre-flight step (config, cwd, resolution, execution)
            // funnels into one `Result<ExecData, CliError>` so `emit` owns the
            // stdout/stderr/exit-code triad — nothing bubbles out of `main` as an
            // exit-1 anyhow error that would carry no envelope under `--json`.
            let cli_vars = vars_map(&cli.vars);
            let result = async {
                let runtime = build_runtime_cfg()?;
                let cwd = headless_cwd()?;
                let args = run_cmd::RunArgs {
                    endpoint_path: endpoint,
                    cli_vars,
                    profile: cli.profile.clone(),
                    proxy: cli.proxy.clone(),
                    insecure: cli.insecure,
                    verbose,
                    cli_asserts: assert,
                };
                run_cmd::run(args, &cwd, &runtime).await
            }
            .await;
            let code = output::emit("run", json, result, |data| {
                headless::print_human(data, verbose)
            });
            std::process::exit(code);
        }
        Some(Command::RunSeq { name, verbose }) => {
            // Streaming command: it owns its stdout/stderr/exit-code directly
            // (pre-flight errors emit a single envelope, per-step results ride
            // the NDJSON stream), so it does not funnel through `emit`.
            let cli_vars = vars_map(&cli.vars);
            let code = match build_runtime_cfg() {
                Ok(runtime) => match headless_cwd() {
                    Ok(cwd) => {
                        let args = seq_cmd::SeqArgs {
                            name,
                            cli_vars,
                            profile: cli.profile.clone(),
                            proxy: cli.proxy.clone(),
                            insecure: cli.insecure,
                            verbose,
                        };
                        seq_cmd::run(args, &cwd, &runtime, json).await
                    }
                    Err(err) => output::emit_error("run-seq", json, err),
                },
                Err(err) => output::emit_error("run-seq", json, err),
            };
            std::process::exit(code);
        }
        Some(Command::Load {
            endpoint,
            total,
            concurrency,
            gap,
            assert,
        }) => {
            // Same funnel as `run`: every fallible pre-flight step lands in one
            // `Result<LoadData, CliError>` so `emit` owns the
            // stdout/stderr/exit-code triad (a single aggregate envelope, not a
            // stream).
            let cli_vars = vars_map(&cli.vars);
            let result = async {
                let runtime = build_runtime_cfg()?;
                let cwd = headless_cwd()?;
                let args = load_cmd::LoadArgs {
                    endpoint_path: endpoint,
                    total,
                    concurrency,
                    gap_ms: gap,
                    cli_vars,
                    profile: cli.profile.clone(),
                    proxy: cli.proxy.clone(),
                    insecure: cli.insecure,
                    cli_asserts: assert,
                };
                load_cmd::run(args, &cwd, &runtime).await
            }
            .await;
            let code = output::emit("load", json, result, load_cmd::print_human);
            std::process::exit(code);
        }
        Some(Command::Send {
            url_pos,
            url,
            method,
            header,
            body,
            verbose,
            assert,
        }) => {
            let cli_vars = vars_map(&cli.vars);
            let result = async {
                let runtime = build_runtime_cfg()?;
                let cwd = headless_cwd()?;
                let args = send_cmd::SendArgs {
                    url: url_pos
                        .or(url)
                        .expect("clap enforces exactly one of url_pos/url"),
                    method,
                    headers: header,
                    body,
                    cli_vars,
                    profile: cli.profile.clone(),
                    proxy: cli.proxy.clone(),
                    insecure: cli.insecure,
                    verbose,
                    cli_asserts: assert,
                };
                send_cmd::run(args, &cwd, &runtime).await
            }
            .await;
            let code = output::emit("send", json, result, |data| {
                headless::print_human(data, verbose)
            });
            std::process::exit(code);
        }
        Some(Command::Keymaps) => {
            run_keymaps()?;
        }
        Some(Command::Cookies { action }) => {
            run_cookies(action)?;
        }
        Some(Command::Init { path, demo }) => {
            init::run_init(path, demo)?;
        }
        Some(Command::Completions { shell }) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_owned();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
        Some(Command::Man) => {
            let cmd = Cli::command();
            clap_mangen::Man::new(cmd).render(&mut std::io::stdout())?;
        }
        Some(Command::Update { check, yes }) => {
            update::run_update(check, yes).await?;
        }
        Some(Command::Uninstall { purge, yes }) => {
            uninstall::run_uninstall(purge, yes)?;
        }
        None => {
            let vars = vars_map(&cli.vars);
            // Import into the cwd workspace *before* the TUI launches (fail loudly
            // on a bad file — never launch a half-imported TUI silently).
            if let Some(file) = &cli.import_collection {
                let cwd = std::env::current_dir()?;
                match import_collection_into(&cwd, file) {
                    Ok(summary) => {
                        println!(
                            "imported {} endpoint(s) into {} collection(s)",
                            summary.endpoints, summary.collections
                        );
                        for warning in &summary.warnings {
                            eprintln!("warning: {warning}");
                        }
                    }
                    Err(err) => {
                        eprintln!("error: import failed: {err}");
                        std::process::exit(1);
                    }
                }
            }
            install_hooks()?;
            churl::tui::run(vars, cli.profile, cli.proxy, cli.insecure).await?;
        }
    }

    Ok(())
}

/// Reads a collection JSON file (Postman v2.1 or churl-native, auto-detected)
/// and writes its endpoints into the workspace rooted at `root`. Shared,
/// testable seam behind `--import-collection` (the in-TUI import path uses the
/// same [`churl_core::interchange::write_import`] core helper). Returns a
/// summary.
fn import_collection_into(
    root: &std::path::Path,
    file: &std::path::Path,
) -> Result<churl_core::interchange::ImportSummary> {
    let json = std::fs::read_to_string(file)
        .map_err(|err| eyre!("cannot read {}: {err}", file.display()))?;
    let import = churl_core::interchange::import_json(&json)?;
    let summary = churl_core::interchange::write_import(root, &import)?;
    Ok(summary)
}

/// `churl keymaps`: print the effective keymap (defaults + config overrides) as
/// plain aligned text, sorted by action config-name, one line per action:
/// `action-name    combo[, combo…]    (default | overridden)`.
fn run_keymaps() -> Result<()> {
    use churl::tui::events::{Action, KeyMap, PaneCtx};

    let config = churl_core::config::load_global_config()?;
    let effective = KeyMap::with_all_overrides(&config.keys, &config.key_overlays)?;
    let default = KeyMap::default();
    // Load-time conflict/shadow warnings — printed as a trailing section.
    let warnings = effective.validate(&config.keys, &config.key_overlays);

    let mut actions: Vec<Action> = Action::all().collect();
    actions.sort_by_key(|a| a.name());
    let name_w = actions.iter().map(|a| a.name().len()).max().unwrap_or(0);

    let fmt_combos = |combos: Vec<String>| {
        if combos.is_empty() {
            "(unbound)".to_owned()
        } else {
            combos.join(", ")
        }
    };

    // Global bindings first: every action (unbound ones included), unindented so
    // each line starts with the action name.
    let combo_w = actions
        .iter()
        .map(|a| fmt_combos(effective.combos_for(*a)).len())
        .max()
        .unwrap_or(0);
    for action in &actions {
        let origin = if effective.combos_for(*action) == default.combos_for(*action) {
            "default"
        } else {
            "overridden"
        };
        println!(
            "{name:<name_w$}  {combos:<combo_w$}  ({origin})  {label}",
            name = action.name(),
            combos = fmt_combos(effective.combos_for(*action)),
            label = action.label(),
        );
    }

    // Per-pane overlays under their headers.
    for ctx in PaneCtx::all() {
        let overlay: Vec<_> = actions
            .iter()
            .filter(|a| !effective.overlay_combos_for(ctx, **a).is_empty())
            .collect();
        if overlay.is_empty() {
            continue;
        }
        println!("\n{}", ctx.header());
        for action in overlay {
            let combos = effective.overlay_combos_for(ctx, *action);
            let origin = if effective.overlay_combos_for(ctx, *action)
                == default.overlay_combos_for(ctx, *action)
            {
                "default"
            } else {
                "overridden"
            };
            println!(
                "  {name:<name_w$}  {combos:<combo_w$}  ({origin})  {label}",
                name = action.name(),
                combos = fmt_combos(combos),
                label = action.label(),
            );
        }
    }

    // Leader continuations under their own header.
    let leader: Vec<_> = actions
        .iter()
        .filter(|a| !effective.leader_combos_for(**a).is_empty())
        .collect();
    if !leader.is_empty() {
        println!("\nLeader");
        for action in leader {
            let combos = effective.leader_combos_for(*action);
            let origin =
                if effective.leader_combos_for(*action) == default.leader_combos_for(*action) {
                    "default"
                } else {
                    "overridden"
                };
            println!(
                "  {name:<name_w$}  {combos:<combo_w$}  ({origin})  {label}",
                name = action.name(),
                combos = fmt_combos(combos),
                label = action.label(),
            );
        }
    }

    // Conflict/shadow warnings, if any — a trailing loud section.
    if !warnings.is_empty() {
        println!("\n⚠ Conflicts");
        for warning in &warnings {
            println!("  {warning}");
        }
    }
    Ok(())
}

/// `churl cookies list|clear`: headless inspection/clearing of the current
/// workspace's persistent cookie jar in `state.sqlite` (keyed by the
/// canonicalized workspace root). Cookie values are printed verbatim by `list`
/// (this is an explicit, user-invoked dump — no masking) so the output is
/// scriptable; `clear` wipes the stored blob.
fn run_cookies(action: CookiesAction) -> Result<()> {
    use churl_core::cookies::ChurlCookieJar;
    use churl_core::history::{HistoryStore, default_state_path};

    let path = default_state_path().ok_or_else(|| eyre!("no data directory for cookie store"))?;
    let store = HistoryStore::open(&path)?;
    let cwd = std::env::current_dir()?;
    let key = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.clone())
        .to_string_lossy()
        .into_owned();

    match action {
        CookiesAction::List => {
            let jar = match store.cookie_jar(&key)? {
                Some(json) => ChurlCookieJar::load_json(&json)?,
                None => ChurlCookieJar::new(),
            };
            let cookies = jar.list();
            if cookies.is_empty() {
                eprintln!("no cookies stored for {}", cwd.display());
            } else {
                for c in cookies {
                    println!("{}\t{}\t{}", c.domain, c.name, c.value);
                }
            }
        }
        CookiesAction::Clear => {
            store.save_cookie_jar(&key, "", now_ms())?;
            eprintln!("cleared cookies for {}", cwd.display());
        }
    }
    Ok(())
}

/// Unix-milliseconds now (for the cookie-jar `updated_at` column).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Where `churl import` landed the endpoint — the `written` field of the
/// import envelope's `data`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WrittenTo {
    /// `--stdout`: the endpoint TOML, not written to disk.
    Stdout { toml: String },
    /// `--out FILE`: written to an arbitrary path outside any workspace.
    File { path: String },
    /// Default: written into the cwd workspace via the normal endpoint CRUD
    /// seam (auto-slugged, collision-suffixed filename).
    Workspace { path: String },
}

/// The `data` payload of a successful `import` envelope.
#[derive(Debug, Serialize)]
struct ImportData {
    name: String,
    method: String,
    url: String,
    warnings: Vec<String>,
    written: WrittenTo,
}

/// `import` has no assertions to fail on — always exit 0 on success (the
/// `SuccessExitCode` default).
impl output::SuccessExitCode for ImportData {}

/// Maps a curl-parse failure onto the frozen `not-a-curl-command` slug (band
/// 5) — every [`churl_core::import::ImportError`] variant is, from the
/// caller's point of view, "the input you gave me is not a curl command I can
/// import."
fn from_import_error(err: churl_core::import::ImportError) -> CliError {
    CliError::new(ErrorKind::NotACurlCommand, err.to_string())
}

/// `churl import`: parses `curl_input` (trailing tokens from the CLI — either
/// zero/`-` for stdin, one quoted string, or several raw shell-tokenised
/// args), then writes the resulting endpoint per `stdout`/`out`/the cwd
/// workspace default, and prints/emits the result. Returns the process exit
/// code.
///
/// The CLI has no live session, so any placeholdered secret (`{{token}}` /
/// `{{password}}`) stays a placeholder — the `no secrets in workspace files`
/// warning already tells the user to bind it via a profile/env. The real secret
/// value in `captured_secrets` is deliberately NOT printed or emitted.
fn run_import(
    curl_input: Vec<String>,
    name: Option<String>,
    stdout: bool,
    out: Option<PathBuf>,
    json: bool,
) -> i32 {
    // Dispatch on how many trailing tokens the shell handed us:
    //   • 0 tokens (or a lone `-`) → read the whole command from stdin
    //     (multi-line paste survives without the shell mangling it). Guard
    //     against a hang: reading a TTY blocks forever on Ctrl-D, so refuse
    //     with a usage hint when stdin is an interactive terminal — never
    //     block waiting for input that will never come (non-interactive
    //     guarantee).
    //   • exactly 1 token → a full command STRING → `import_curl` (shlex).
    //   • >1 tokens → the shell already tokenised the curl → parse the
    //     tokens directly (no re-tokenising) via `import_curl_tokens`.
    let is_stdin = curl_input.is_empty() || (curl_input.len() == 1 && curl_input[0] == "-");
    let parsed: Result<churl_core::import::ImportResult, CliError> = if is_stdin {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            Err(CliError::new(
                ErrorKind::NotACurlCommand,
                "no curl command given: provide it as an argument, or pipe one in — \
                 e.g. `pbpaste | churl import -`",
            ))
        } else {
            let mut buf = String::new();
            match std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
                Ok(_) => churl_core::import::import_curl(&buf).map_err(from_import_error),
                Err(err) => Err(CliError::new(
                    ErrorKind::NotACurlCommand,
                    format!("failed to read stdin: {err}"),
                )),
            }
        }
    } else if curl_input.len() == 1 {
        churl_core::import::import_curl(&curl_input[0]).map_err(from_import_error)
    } else {
        churl_core::import::import_curl_tokens(curl_input).map_err(from_import_error)
    };

    let result: Result<ImportData, CliError> = parsed.and_then(|parsed| {
        let mut endpoint = parsed.endpoint;
        if let Some(name) = name {
            endpoint.name = name;
        }
        let mut warnings = parsed.warnings;

        let written = if stdout {
            let toml = churl_core::persistence::endpoint_to_toml(&endpoint)
                .map_err(|err| CliError::new(ErrorKind::ImportWriteFailed, err.to_string()))?;
            WrittenTo::Stdout { toml }
        } else if let Some(path) = out {
            churl_core::persistence::save_endpoint(&path, &endpoint)
                .map_err(|err| CliError::new(ErrorKind::ImportWriteFailed, err.to_string()))?;
            WrittenTo::File {
                path: path.display().to_string(),
            }
        } else {
            let cwd = std::env::current_dir()
                .map_err(|err| CliError::new(ErrorKind::ImportWriteFailed, err.to_string()))?;
            let workspace = churl::tui::app::open_workspace(&cwd)
                .map_err(|err| CliError::new(ErrorKind::NoWorkspace, err.to_string()))?
                .ok_or_else(|| {
                    CliError::new(
                        ErrorKind::NoWorkspace,
                        format!(
                            "no churl workspace at {} (no churl.toml) — run `churl init` \
                             first, or use --stdout/--out",
                            cwd.display()
                        ),
                    )
                })?;
            // Single atomic claim+save: a refused literal secret or a disk error
            // leaves the workspace unchanged (no orphaned placeholder). On a
            // filename collision `create_endpoint_with` bumps the stem AND sets
            // the saved `name` to it, so both endpoints stay addressable.
            let requested_slug = churl_core::persistence::slug_of(&endpoint.name);
            let claimed =
                churl_core::persistence::create_endpoint_with(workspace.root(), &endpoint)
                    .map_err(|err| CliError::new(ErrorKind::ImportWriteFailed, err.to_string()))?;
            let claimed_stem = claimed
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if claimed_stem != requested_slug {
                // The name was bumped to avoid a collision — reflect the saved,
                // addressable name in the envelope + warn (loud, never silent).
                warnings.push(format!(
                    "named {claimed_stem:?} to avoid a filename collision with {:?}",
                    endpoint.name
                ));
                endpoint.name = claimed_stem.to_owned();
            }
            WrittenTo::Workspace {
                path: claimed.display().to_string(),
            }
        };

        Ok(ImportData {
            name: endpoint.name.clone(),
            method: endpoint.request.method.to_string(),
            url: endpoint.request.url.clone(),
            warnings,
            written,
        })
    });

    output::emit("import", json, result, |data| {
        for warning in &data.warnings {
            eprintln!("warning: {warning}");
        }
        match &data.written {
            WrittenTo::Stdout { toml } => print!("{toml}"),
            WrittenTo::File { path } | WrittenTo::Workspace { path } => {
                eprintln!("wrote {path}")
            }
        }
    })
}

/// Install color-eyre panic and error hooks that restore the terminal before
/// printing the report — the standard ratatui + color-eyre recipe.
fn install_hooks() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default().into_hooks();

    // Restore the terminal before the panic hook prints.
    let panic_hook = panic_hook.into_panic_hook();
    std::panic::set_hook(Box::new(move |info| {
        churl::tui::restore();
        panic_hook(info);
    }));

    let eyre_hook = eyre_hook.into_eyre_hook();
    color_eyre::eyre::set_hook(Box::new(move |e| {
        churl::tui::restore();
        eyre_hook(e)
    }))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const FIXTURE: &str = r#"{
        "info": { "name": "Fixture API", "schema": "…v2.1.0…" },
        "item": [
            { "name": "list users", "request": { "method": "GET", "url": { "raw": "https://api.test/users" } } },
            { "name": "folder", "item": [
                { "name": "nested", "request": { "method": "POST", "url": { "raw": "https://api.test/n" },
                    "auth": { "type": "bearer", "bearer": [ { "key": "token", "value": "ghp_literal" } ] } } }
            ] }
        ]
    }"#;

    #[test]
    fn cli_parses_proxy_and_insecure_flags() {
        let cli =
            Cli::try_parse_from(["churl", "--proxy", "http://proxy.local:3128", "-k"]).unwrap();
        assert_eq!(cli.proxy.as_deref(), Some("http://proxy.local:3128"));
        assert!(cli.insecure);
        assert!(cli.command.is_none());

        // Defaults: no proxy, verify on.
        let bare = Cli::try_parse_from(["churl"]).unwrap();
        assert!(bare.proxy.is_none());
        assert!(!bare.insecure);
        assert!(!bare.json);
    }

    #[test]
    fn cli_parses_json_flag() {
        let cli = Cli::try_parse_from(["churl", "--json", "run", "ep"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn cli_parses_cookies_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["churl", "cookies", "list"])
                .unwrap()
                .command,
            Some(Command::Cookies {
                action: CookiesAction::List
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["churl", "cookies", "clear"])
                .unwrap()
                .command,
            Some(Command::Cookies {
                action: CookiesAction::Clear
            })
        ));
    }

    #[test]
    fn cli_parses_global_import_collection_flag() {
        let cli = Cli::try_parse_from(["churl", "--import-collection", "coll.json"]).unwrap();
        assert_eq!(
            cli.import_collection.as_deref(),
            Some(std::path::Path::new("coll.json"))
        );
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_send_requires_a_url() {
        // Neither positional nor --url: clap's own usage error (exit 2 band).
        assert!(Cli::try_parse_from(["churl", "send"]).is_err());
        // Exactly one is fine, either form.
        assert!(Cli::try_parse_from(["churl", "send", "https://x.test"]).is_ok());
        assert!(Cli::try_parse_from(["churl", "send", "--url", "https://x.test"]).is_ok());
        // Both at once is a usage error too.
        assert!(
            Cli::try_parse_from(["churl", "send", "https://x.test", "--url", "https://y.test"])
                .is_err()
        );
    }

    #[test]
    fn cli_send_parses_curl_mnemonic_and_churl_native_flags() {
        let cli = Cli::try_parse_from([
            "churl",
            "send",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            "{}",
            "https://x.test",
        ])
        .unwrap();
        let Some(Command::Send {
            method,
            header,
            body,
            url_pos,
            ..
        }) = cli.command
        else {
            panic!("expected Send");
        };
        assert_eq!(method, Some(churl_core::model::Method::Post));
        assert_eq!(
            header,
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
        assert_eq!(body.as_deref(), Some("{}"));
        assert_eq!(url_pos.as_deref(), Some("https://x.test"));

        let native = Cli::try_parse_from([
            "churl",
            "send",
            "--method",
            "PUT",
            "--header",
            "X-A: b",
            "--body",
            "hi",
            "--url",
            "https://x.test",
        ])
        .unwrap();
        let Some(Command::Send { method, header, .. }) = native.command else {
            panic!("expected Send");
        };
        assert_eq!(method, Some(churl_core::model::Method::Put));
        assert_eq!(header, vec![("X-A".to_owned(), "b".to_owned())]);
    }

    #[test]
    fn import_collection_into_writes_endpoints_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("collection.json");
        std::fs::write(&file, FIXTURE).unwrap();

        let summary = import_collection_into(root, &file).unwrap();
        assert_eq!(summary.endpoints, 2);
        assert_eq!(summary.collections, 2);
        // The bearer secret was placeholder-ized → a warning fired.
        assert!(
            summary.warnings.iter().any(|w| w.contains("placeholder")),
            "{:?}",
            summary.warnings
        );

        // Root-level request lands in a collection named after the import.
        let flat = root.join("fixture-api").join("list-users.toml");
        assert!(flat.exists(), "missing {}", flat.display());
        let toml = std::fs::read_to_string(&flat).unwrap();
        assert!(toml.contains(r#"name = "list users""#), "{toml}");
        assert!(toml.contains(r#"url = "https://api.test/users""#), "{toml}");

        // Nested folder request lands in a "folder" collection, secret masked.
        let nested = root.join("folder").join("nested.toml");
        assert!(nested.exists(), "missing {}", nested.display());
        let nested_toml = std::fs::read_to_string(&nested).unwrap();
        assert!(nested_toml.contains("{{token}}"), "{nested_toml}");
        assert!(
            !nested_toml.contains("ghp_literal"),
            "literal secret leaked: {nested_toml}"
        );
    }

    const NATIVE_FIXTURE: &str = r#"{
        "churl_version": 1,
        "name": "Native API",
        "collections": [
            { "name": "users", "endpoints": [
                { "name": "list", "request": { "method": "GET", "url": "https://api.test/users" } },
                { "name": "make", "request": { "method": "POST", "url": "https://api.test/users",
                    "auth": { "type": "bearer", "token": "{{token}}" } } }
            ] }
        ]
    }"#;

    #[test]
    fn import_collection_into_auto_detects_native_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("native.json");
        std::fs::write(&file, NATIVE_FIXTURE).unwrap();

        let summary = import_collection_into(root, &file).unwrap();
        assert_eq!(summary.endpoints, 2);
        assert_eq!(summary.collections, 1);

        // Native endpoints land under a collection named for their group ("users").
        let listed = root.join("users").join("list.toml");
        assert!(listed.exists(), "missing {}", listed.display());
        let ep = churl_core::persistence::load_endpoint(&listed).unwrap();
        assert_eq!(ep.name, "list");
        assert_eq!(ep.request.url, "https://api.test/users");
    }

    #[test]
    fn import_collection_into_rejects_bad_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(import_collection_into(dir.path(), &file).is_err());
    }
}
