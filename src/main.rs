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
    Doctor {
        /// Try to install missing tools (or print manual steps)
        #[arg(long)]
        fix: bool,
    },
    /// Hop into the usagi welcome screen
    Hop,
    /// Register the current directory as a project (or clone one into it with --git)
    Init {
        /// Clone this repository URL into <repo-name>/ under the current directory
        #[arg(long, value_name = "URL")]
        git: Option<String>,
    },
    /// Sync the current repository's worktree state to .usagi/state.json
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Hop => usagi::presentation::cli::hop::run(),
        Commands::Init { git } => usagi::presentation::cli::init::run(git),
        Commands::Status => usagi::presentation::cli::status::run(),
    }
}
