//! 人間向け CLI サブコマンドのハンドラ置き場。1 コマンド = 1 ハンドラ型とし、
//! それぞれが `Run` を実装する。`cli/mod.rs` の dispatch（`Command::into_handler`）が
//! 解釈済みコマンドを対応ハンドラに変換し、実行は `Run::run` の一様な呼び出しになる。
//!
//! 各ハンドラは presentation に徹する — 解析済みのオプションを保持し、store 系は
//! usagi-core の usecase を直接呼び、session 系は usagi-core の IPC クライアント経由で
//! daemon に委譲し、結果を整形して返す（独自のビジネスロジックは持たない）。
//! MCP の tool アダプタ（`crate::mcp::tools`）は同じ core usecase を呼ぶ兄弟である。
//!
//! 現状は **コマンド面の枠だけ** を実装しており、各ハンドラは未実装を報告する
//! スタブである。v2 では必要になった時点で中身を実装する。

use std::io::{self, Write};

use super::{IssueCommand, MemoryCommand, OpCommand, Run, Shell};

/// 未実装のサブコマンドを表す共通のスタブ出力を `out` に書く。
///
/// `detail` が空でなければ括弧付きで解析済みオプションを併記し、コマンド面の枠が
/// オプションまで通っていることを示す。
fn unimplemented(out: &mut dyn Write, command: &str, detail: &str) -> io::Result<()> {
    if detail.is_empty() {
        writeln!(out, "usagi {command}: not yet implemented")
    } else {
        writeln!(out, "usagi {command}: not yet implemented ({detail})")
    }
}

/// `usagi init [--git <URL>]`。
pub struct Init {
    pub git: Option<String>,
}

impl Run for Init {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let detail = match &self.git {
            Some(url) => format!("git={url}"),
            None => String::new(),
        };
        unimplemented(out, "init", &detail)
    }
}

/// `usagi init-agent [-y]`。
pub struct InitAgent {
    pub yes: bool,
}

impl Run for InitAgent {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "init-agent", &format!("yes={}", self.yes))
    }
}

/// `usagi status`。
pub struct Status;

impl Run for Status {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "status", "")
    }
}

/// `usagi update [--dry-run]`。
pub struct Update {
    pub dry_run: bool,
}

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "update", &format!("dry_run={}", self.dry_run))
    }
}

/// `usagi clean [--dry-run] [--agent <NAME>]`。
pub struct Clean {
    pub dry_run: bool,
    pub agent: Option<String>,
}

impl Run for Clean {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let agent = self.agent.as_deref().unwrap_or("<default>");
        unimplemented(
            out,
            "clean",
            &format!("dry_run={}, agent={agent}", self.dry_run),
        )
    }
}

/// `usagi doctor [--fix]`。
pub struct Doctor {
    pub fix: bool,
}

impl Run for Doctor {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "doctor", &format!("fix={}", self.fix))
    }
}

/// `usagi feature`。
pub struct Feature;

impl Run for Feature {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "feature", "")
    }
}

/// `usagi completion <shell>`。
pub struct Completion {
    pub shell: Shell,
}

impl Run for Completion {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let shell = match self.shell {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Powershell => "powershell",
            Shell::Elvish => "elvish",
        };
        unimplemented(out, "completion", &format!("shell={shell}"))
    }
}

/// `usagi op <subcommand>`。
pub struct Op {
    pub command: OpCommand,
}

impl Run for Op {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let sub = match self.command {
            OpCommand::Login => "login",
        };
        unimplemented(out, &format!("op {sub}"), "")
    }
}

/// `usagi config [--edit]`。
pub struct Config {
    pub edit: bool,
}

impl Run for Config {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "config", &format!("edit={}", self.edit))
    }
}

/// `usagi issue <subcommand>`。
pub struct Issue {
    pub command: IssueCommand,
}

impl Run for Issue {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let sub = match &self.command {
            IssueCommand::Create { .. } => "create",
            IssueCommand::List => "list",
            IssueCommand::Graph => "graph",
            IssueCommand::Show { .. } => "show",
            IssueCommand::Update { .. } => "update",
            IssueCommand::Search { .. } => "search",
            IssueCommand::Delete { .. } => "delete",
        };
        unimplemented(out, &format!("issue {sub}"), "")
    }
}

/// `usagi memory <subcommand>`。
pub struct Memory {
    pub command: MemoryCommand,
}

impl Run for Memory {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let sub = match &self.command {
            MemoryCommand::Save { .. } => "save",
            MemoryCommand::List => "list",
            MemoryCommand::Show { .. } => "show",
            MemoryCommand::Update { .. } => "update",
            MemoryCommand::Search { .. } => "search",
            MemoryCommand::Delete { .. } => "delete",
        };
        unimplemented(out, &format!("memory {sub}"), "")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Command, IssueCommand, MemoryCommand, OpCommand, Shell};

    /// コマンドを dispatch してハンドラを実行し、出力文字列を得るヘルパ。
    fn render(command: Command) -> String {
        let mut out = Vec::new();
        command.into_handler().run(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn init_reports_git_option() {
        let with = render(Command::Init {
            git: Some("u".into()),
        });
        assert!(with.contains("init") && with.contains("git=u"));
        let without = render(Command::Init { git: None });
        assert!(without.contains("init") && !without.contains('('));
    }

    #[test]
    fn simple_and_flag_handlers_report_their_names() {
        assert!(render(Command::Status).contains("status"));
        assert!(render(Command::Feature).contains("feature"));
        assert!(render(Command::InitAgent { yes: true }).contains("yes=true"));
        assert!(render(Command::Update { dry_run: true }).contains("dry_run=true"));
        assert!(render(Command::Doctor { fix: false }).contains("fix=false"));
        assert!(render(Command::Config { edit: true }).contains("edit=true"));
    }

    #[test]
    fn clean_defaults_agent_label() {
        let default = render(Command::Clean {
            dry_run: false,
            agent: None,
        });
        assert!(default.contains("agent=<default>"));
        let named = render(Command::Clean {
            dry_run: true,
            agent: Some("codex".into()),
        });
        assert!(named.contains("agent=codex") && named.contains("dry_run=true"));
    }

    #[test]
    fn completion_maps_every_shell() {
        for (shell, label) in [
            (Shell::Bash, "bash"),
            (Shell::Zsh, "zsh"),
            (Shell::Fish, "fish"),
            (Shell::Powershell, "powershell"),
            (Shell::Elvish, "elvish"),
        ] {
            let out = render(Command::Completion { shell });
            assert!(out.contains(label), "expected {label} in {out}");
        }
    }

    #[test]
    fn op_dispatches_its_subcommand() {
        assert!(
            render(Command::Op {
                command: OpCommand::Login
            })
            .contains("op login")
        );
    }

    #[test]
    fn issue_dispatches_every_subcommand() {
        let cases = [
            (IssueCommand::Create { title: "t".into() }, "issue create"),
            (IssueCommand::List, "issue list"),
            (IssueCommand::Graph, "issue graph"),
            (IssueCommand::Show { number: 1 }, "issue show"),
            (
                IssueCommand::Update {
                    number: 1,
                    status: None,
                },
                "issue update",
            ),
            (IssueCommand::Search { query: "q".into() }, "issue search"),
            (IssueCommand::Delete { number: 1 }, "issue delete"),
        ];
        for (command, expected) in cases {
            let out = render(Command::Issue { command });
            assert!(out.contains(expected), "expected {expected} in {out}");
        }
    }

    #[test]
    fn memory_dispatches_every_subcommand() {
        let cases = [
            (MemoryCommand::Save { name: "n".into() }, "memory save"),
            (MemoryCommand::List, "memory list"),
            (MemoryCommand::Show { name: "n".into() }, "memory show"),
            (MemoryCommand::Update { name: "n".into() }, "memory update"),
            (MemoryCommand::Search { query: "q".into() }, "memory search"),
            (MemoryCommand::Delete { name: "n".into() }, "memory delete"),
        ];
        for (command, expected) in cases {
            let out = render(Command::Memory { command });
            assert!(out.contains(expected), "expected {expected} in {out}");
        }
    }
}
