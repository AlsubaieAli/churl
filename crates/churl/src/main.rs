use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

#[derive(Debug, Parser)]
#[command(name = "churl", about = "Terminal HTTP client", version)]
struct Cli {
    /// Set a template variable (repeatable): `--var key=value`. Highest-precedence
    /// scope in the `{{var}}` resolver, above profiles and workspace vars.
    #[arg(long = "var", value_name = "KEY=VALUE", global = true)]
    vars: Vec<String>,
    /// Activate a named profile at startup (unknown name is an error).
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Import a curl command as an endpoint (prints TOML; --out writes a file)
    Import {
        /// The curl command to import (quote the whole command)
        curl: String,
        /// Override the endpoint name derived from the URL
        #[arg(long)]
        name: Option<String>,
        /// Write the endpoint TOML to this file instead of stdout
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Print the effective keymap (every action, its bindings, and default/overridden)
    Keymaps,
}

/// Parses `--var key=value` pairs into a map. A missing `=` is a hard error.
fn parse_vars(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| eyre!("bad --var {pair:?}: expected key=value"))?;
        vars.insert(key.to_owned(), value.to_owned());
    }
    Ok(vars)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Import { curl, name, out }) => {
            run_import(&curl, name, out)?;
        }
        Some(Command::Keymaps) => {
            run_keymaps()?;
        }
        None => {
            let vars = parse_vars(&cli.vars)?;
            install_hooks()?;
            churl::tui::run(vars, cli.profile).await?;
        }
    }

    Ok(())
}

/// `churl keymaps`: print the effective keymap (defaults + config overrides) as
/// plain aligned text, sorted by action config-name, one line per action:
/// `action-name    combo[, combo…]    (default | overridden)`.
fn run_keymaps() -> Result<()> {
    use churl::tui::events::{Action, KeyMap, PaneCtx};

    let config = churl_core::config::load_global_config()?;
    let effective = KeyMap::with_all_overrides(&config.keys, &config.key_overlays)?;
    let default = KeyMap::default();

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
    Ok(())
}

/// `churl import`: parse the curl command, print the endpoint's TOML to stdout
/// (warnings on stderr), or write it through the persistence save with `--out`.
/// A parse failure prints the error on stderr and exits non-zero.
fn run_import(curl: &str, name: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let result = match churl_core::import::import_curl(curl) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let mut endpoint = result.endpoint;
    if let Some(name) = name {
        endpoint.name = name;
    }
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    match out {
        Some(path) => {
            churl_core::persistence::save_endpoint(&path, &endpoint)?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{}", churl_core::persistence::endpoint_to_toml(&endpoint)?),
    }
    Ok(())
}

/// Install color-eyre panic and error hooks that restore the terminal before
/// printing the report — the standard ratatui + color-eyre recipe.
fn install_hooks() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default().into_hooks();

    // Wrap the panic hook so the terminal is restored first.
    let panic_hook = panic_hook.into_panic_hook();
    std::panic::set_hook(Box::new(move |info| {
        churl::tui::restore();
        panic_hook(info);
    }));

    // Wrap the eyre hook similarly.
    let eyre_hook = eyre_hook.into_eyre_hook();
    color_eyre::eyre::set_hook(Box::new(move |e| {
        churl::tui::restore();
        eyre_hook(e)
    }))?;

    Ok(())
}
