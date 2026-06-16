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
    /// Show usagi's configuration (or edit it with --edit)
    Config {
        /// Open the configuration file in $EDITOR and validate it on save
        #[arg(long)]
        edit: bool,
    },
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
    /// Manage task issues stored in .usagi/issues/
    Issue {
        #[command(subcommand)]
        command: usagi::presentation::cli::issue::IssueCommand,
    },
    /// Run the local LLM MCP server over stdio (for AI agents to offload work)
    LlmMcp {
        /// The Ollama model completions run against
        #[arg(long, value_name = "MODEL", default_value = usagi::domain::settings::DEFAULT_LOCAL_LLM_MODEL)]
        model: String,
    },
    /// Run the issue MCP server over stdio (for AI agents)
    Mcp,
    /// Run the session orchestration MCP server over stdio (for AI agents to
    /// create sessions and delegate prompts to them)
    SessionMcp,
    /// Sync the current repository's worktree state to .usagi/state.json
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Config { edit } => usagi::presentation::cli::config::run(edit),
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Hop => usagi::presentation::cli::hop::run(),
        Commands::Init { git } => usagi::presentation::cli::init::run(git),
        Commands::Issue { command } => usagi::presentation::cli::issue::run(command),
        Commands::LlmMcp { model } => usagi::presentation::cli::llm_mcp::run(model),
        Commands::Mcp => usagi::presentation::cli::mcp::run(),
        Commands::SessionMcp => usagi::presentation::cli::session_mcp::run(),
        Commands::Status => usagi::presentation::cli::status::run(),
    }
}
