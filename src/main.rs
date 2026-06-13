use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "usagi",
    version,
    about = "TUI/CLI for managing AI agent workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check that required tools are installed
    Doctor,
    /// Hop into the usagi startup screen
    Hop,
    /// Sync the current repository's worktree state to .usagi/state.json
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Doctor => usagi::presentation::cli::doctor::run(),
        Commands::Hop => usagi::presentation::cli::hop::run(),
        Commands::Status => usagi::presentation::cli::status::run(),
    }
}
