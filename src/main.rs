use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "usagi",
    version,
    about = "TUI/CLI for managing AI agent workflows"
)]
struct Cli {
    /// Defaults to `hop` (the welcome screen) when no subcommand is given.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Record a running agent's lifecycle phase (invoked by agent hooks)
    #[command(hide = true)]
    AgentPhase {
        /// The phase the agent's hook is reporting
        #[arg(value_enum)]
        phase: usagi::presentation::cli::agent_phase::Phase,
    },
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
    /// Show which usagi features each agent CLI supports
    Feature,
    /// Hop into the usagi welcome screen
    Hop,
    /// Print the square-pixel usagi marks (primary / flip / legibility)
    Icon {
        /// Which mark to show (defaults to all)
        #[arg(value_enum, default_value = "all")]
        view: usagi::presentation::cli::icon::IconView,
    },
    /// Register the current directory as a project (or clone one into it with --git)
    Init {
        /// Clone this repository URL into <repo-name>/ under the current directory
        #[arg(long, value_name = "URL")]
        git: Option<String>,
    },
    /// Manage task issues stored in .usagi/issues/
    ///
    /// Hidden from the CLI: issues are operated by AI agents via the MCP server.
    #[command(hide = true)]
    Issue {
        #[command(subcommand)]
        command: usagi::presentation::cli::issue::IssueCommand,
    },
    /// Manage durable agent memories stored in .usagi/memory/
    ///
    /// Hidden from the CLI: memories are operated by AI agents via the MCP server.
    #[command(hide = true)]
    Memory {
        #[command(subcommand)]
        command: usagi::presentation::cli::memory::MemoryCommand,
    },
    /// Run the local LLM MCP server over stdio (for AI agents to offload work)
    ///
    /// Hidden from the CLI: launched by AI agents, not invoked by hand.
    #[command(hide = true)]
    LlmMcp {
        /// The Ollama model completions run against
        #[arg(long, value_name = "MODEL", default_value = usagi::domain::settings::DEFAULT_LOCAL_LLM_MODEL)]
        model: String,
    },
    /// Run the usagi MCP server over stdio (issue / memory / session tools for AI agents)
    ///
    /// Hidden from the CLI: launched by AI agents, not invoked by hand.
    #[command(hide = true)]
    Mcp,
    /// Play a usagi animation (1=走る 2=増える 3,4=読み込み 5=マスコット)
    Run {
        /// Which animation to play (1–5)
        #[arg(value_name = "N", default_value_t = 1)]
        n: u8,
    },
    /// Sync the current repository's worktree state to .usagi/state.json
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // No subcommand behaves the same as `usagi hop`.
    let command = cli.command.unwrap_or(Commands::Hop);

    let result = match command {
        Commands::AgentPhase { phase } => usagi::presentation::cli::agent_phase::run(phase),
        Commands::Config { edit } => usagi::presentation::cli::config::run(edit),
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Feature => usagi::presentation::cli::feature::run(),
        Commands::Hop => usagi::presentation::cli::hop::run(),
        Commands::Icon { view } => usagi::presentation::cli::icon::run(view),
        Commands::Init { git } => usagi::presentation::cli::init::run(git),
        Commands::Issue { command } => usagi::presentation::cli::issue::run(command),
        Commands::Memory { command } => usagi::presentation::cli::memory::run(command),
        Commands::LlmMcp { model } => usagi::presentation::cli::llm_mcp::run(model),
        Commands::Mcp => usagi::presentation::cli::mcp::run(),
        Commands::Run { n } => usagi::presentation::cli::run::run(n),
        Commands::Status => usagi::presentation::cli::status::run(),
    };

    if let Err(error) = &result {
        log_error(error);
    }
    result
}

/// Best-effort: append `error` (with its full cause chain) to today's log file
/// and prune files older than the retention window. Any failure here is
/// swallowed so logging never masks the original error on its way to stderr.
fn log_error(error: &anyhow::Error) {
    usagi::infrastructure::error_log::ErrorLog::record(&format!("{error:#}"));
}
