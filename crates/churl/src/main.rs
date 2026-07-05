use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;

#[derive(Debug, Parser)]
#[command(name = "churl", about = "Terminal HTTP client", version)]
struct Cli {
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Import { curl, name, out }) => {
            run_import(&curl, name, out)?;
        }
        None => {
            install_hooks()?;
            churl::tui::run().await?;
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
