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
    /// Import a curl command as an endpoint (not yet implemented)
    Import {
        /// The curl command to import
        curl: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Import { curl: _ }) => {
            eprintln!("curl import: not yet implemented");
            std::process::exit(1);
        }
        None => {
            install_hooks()?;
            churl::tui::run()?;
        }
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
