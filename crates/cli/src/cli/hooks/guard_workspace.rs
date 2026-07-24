//! `usagi guard-workspace` — worktree の外へ出るツール呼び出しを拒否する内部コマンド。
//!
//! usagi がエージェント起動時に Claude の `PreToolUse` フックへ配線し、フックが JSON payload
//! （エージェントの `cwd`・`tool_name`・tool 入力）を stdin で渡して呼ぶ。人手で叩くものでは
//! ない（`--help` 非表示）。malformed・未知・明白に不正な呼び出しを fail closed で拒否する。
//! これは多層防御の一層であり、hard boundary は将来 `claude-sandbox` が入れる OS sandbox が担う。
//!
//! フックは runtime に `cwd` から 2 モードのいずれかを選ぶ。
//!
//! - **session モード**（cwd が `.usagi/sessions/<name>/` 配下）: file 書き込みツールの対象は
//!   既存 symlink を辿って解決し、worktree の外なら拒否する。shell / subagent は shape を検証した
//!   うえで（将来の）OS sandbox に委ねる。
//! - **root モード**（cwd が workspace root。`.usagi/sessions/` 配下ではない）: コーディネータは
//!   リポジトリを一切変更してはならない。ここでは worktree の閉じ込めは効かない（cwd が repo root
//!   そのもので「外」が存在しない）ため、file 書き込みツール（`Edit` / `Write` / `MultiEdit` /
//!   `NotebookEdit`）をパスによらず拒否し、`Bash` は厳格な read-only allowlist 外の command を拒否する。
//!
//! [`crate::mcp`] / system prompt がエージェントに「留まれ」と伝え、このフックが Claude に対して
//! それを強制する。
//!
//! 拒否は Claude Code の `PreToolUse` 契約どおり stdout に `hookSpecificOutput`（`permissionDecision:
//! "deny"`、終了コード 0）で返す（理由も添える）。許可時は何も出力せず、Claude 通常の許可フローに
//! 委ねる。モード／パス／git の判定は [`usagi_core::usecase::workspace_guard`] にあり、ここはその薄い
//! stdin → stdout シムである。

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use usagi_core::usecase::workspace_guard;

use crate::cli::{Run, RunOutcome};

/// `usagi guard-workspace` のハンドラ。実 stdin/stdout は合成ルートが束ねる（[`RunOutcome::GuardWorkspace`]）。
pub struct GuardWorkspace;

impl Run for GuardWorkspace {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::GuardWorkspace)
    }
}

/// `PreToolUse` payload を `input` から読み、ツールの対象がエージェントの worktree を出るときに
/// deny 判定を `output` に書く。read / write を注入することで、プロセスの実 stdin / stdout なしに
/// 判定全体をユニットテストできる（合成ルートが実 stdin / stdout を束ねる）。
///
/// # Errors
///
/// `output` への書き込みに失敗した場合、そのエラーを返す。
pub fn evaluate(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    let mut raw = String::new();
    if let Err(error) = input.read_to_string(&mut raw) {
        return write_denial(output, &format!("guard payload could not be read: {error}"));
    }
    if let Some(reason) = deny_reason(&raw) {
        return write_denial(output, &reason);
    }
    Ok(())
}

fn write_denial(output: &mut dyn Write, reason: &str) -> io::Result<()> {
    // Claude Code の `PreToolUse` deny 契約: `hookSpecificOutput` に deny 判定と理由を載せる。
    // `Value` の Display は失敗しないため、シリアライズは常に成功し `write!` の IO だけが失敗しうる。
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    write!(output, "{payload}")
}

/// このツール呼び出しを拒否する理由。許可なら `None`。モードは payload の canonical な `cwd` から
/// 選ぶ。malformed・不完全な payload はすべて拒否する（フックのパース失敗が許可に化けてはならない）。
fn deny_reason(raw: &str) -> Option<String> {
    let payload: serde_json::Value = match serde_json::from_str(raw) {
        Ok(payload) => payload,
        Err(error) => return Some(format!("malformed PreToolUse payload: {error}")),
    };
    let cwd = match payload.get("cwd").and_then(serde_json::Value::as_str) {
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        _ => return Some("PreToolUse payload has no absolute cwd".to_string()),
    };
    let Ok(cwd) = std::fs::canonicalize(&cwd) else {
        return Some("PreToolUse cwd cannot be canonicalized".to_string());
    };
    let tool_name = match payload.get("tool_name").and_then(serde_json::Value::as_str) {
        Some(name) if !name.is_empty() => name,
        _ => return Some("PreToolUse payload has no tool_name".to_string()),
    };
    let Some(input) = payload
        .get("tool_input")
        .and_then(serde_json::Value::as_object)
    else {
        return Some("PreToolUse payload has no object tool_input".to_string());
    };

    if let Some(worktree) = session_worktree_root(&cwd) {
        session_deny_reason(tool_name, input, &cwd, &worktree)
    } else {
        root_deny_reason(tool_name, input)
    }
}

fn session_worktree_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| {
            let Some(sessions) = candidate.parent() else {
                return false;
            };
            let Some(usagi) = sessions.parent() else {
                return false;
            };
            sessions.file_name() == Some(OsStr::new("sessions"))
                && usagi.file_name() == Some(OsStr::new(".usagi"))
        })
        .map(Path::to_path_buf)
}

/// session モード: file 書き込みの対象を canonicalize し、escape を弾く。既知の非書き込みツールは
/// 通す。shell / subagent の副作用は（将来の）OS sandbox に閉じ込められる。
fn session_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
    worktree: &Path,
) -> Option<String> {
    if workspace_guard::is_write_tool(tool_name) {
        let target = match input.get("file_path").and_then(serde_json::Value::as_str) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => return Some(format!("{tool_name} payload has no file_path")),
        };
        // escape、または解決できないケースは fail-closed で拒否する（判定は core 側で total）。
        if workspace_guard::path_escapes_root(worktree, cwd, &target) {
            return Some(format!(
                "{} はセッション worktree {} の外です。",
                target.display(),
                worktree.display()
            ));
        }
        return None;
    }
    match tool_name {
        // shell command と subagent は必須の OS sandbox を継承する。フックは shape を検証するが、
        // shell semantics を security boundary としてパースするとは主張しない。
        "Bash" => match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) if !command.trim().is_empty() => None,
            _ => Some("Bash payload has no command".to_string()),
        },
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "Task" | "Skill" | "TodoWrite"
        | "AskUserQuestion" => None,
        name if name.starts_with("mcp__") => None,
        _ => Some(format!("unknown tool is denied fail-closed: {tool_name}")),
    }
}

/// root モード: コーディネータはリポジトリを変更してはならない。file 書き込みツールをパスによらず
/// 拒否し、厳格な read-only allowlist 外の shell command をすべて拒否する。
fn root_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if workspace_guard::is_write_tool(tool_name) {
        return Some(format!(
            "ワークスペースルート（コーディネータ）ではファイル書き込みツール（{tool_name}）を実行できません。\
             root 行はリポジトリを変更しません。編集はセッションの worktree に委譲してください。"
        ));
    }
    if tool_name == "Bash" {
        let command = match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) if !command.trim().is_empty() => command,
            _ => return Some("Bash payload has no command".to_string()),
        };
        return workspace_guard::command_mutates_repo(command).then(|| {
            format!(
                "ワークスペースルートでは read-only allowlist 外の shell command を実行できません（{command}）。"
            )
        });
    }
    match tool_name {
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "Task" | "Skill" | "TodoWrite"
        | "AskUserQuestion" => None,
        name if name.starts_with("mcp__") => None,
        _ => Some(format!("unknown tool is denied fail-closed: {tool_name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Command, RunOutcome, execute};
    use std::io::Cursor;

    #[test]
    fn hidden_handler_requests_composition_evaluation_without_output() {
        let (outcome, output) = execute(Command::GuardWorkspace);
        assert_eq!(outcome, RunOutcome::GuardWorkspace);
        assert!(output.is_empty());
    }

    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let worktree = root.join(".usagi/sessions/work");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        (temp, root, worktree)
    }

    fn payload(cwd: &Path, tool_name: &str, input: serde_json::Value) -> String {
        // `input` を値で消費して payload を組む（json! マクロは借用のため clippy が値渡しを警告する）。
        serde_json::Value::Object(serde_json::Map::from_iter([
            ("cwd".to_string(), serde_json::json!(cwd)),
            ("tool_name".to_string(), serde_json::json!(tool_name)),
            ("tool_input".to_string(), input),
        ]))
        .to_string()
    }

    #[test]
    fn denies_a_tool_targeting_the_parent_repo() {
        let (_temp, root, worktree) = layout();
        let target = root.join("src/main.rs");
        let payload = payload(&worktree, "Edit", serde_json::json!({"file_path": target}));
        let mut out = Vec::new();
        evaluate(&mut Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("\"hookEventName\":\"PreToolUse\""));
        // 理由は問題のパスを名指しし、エージェントが避けるべき対象を学べる。
        assert!(written.contains("src/main.rs"));
    }

    #[test]
    fn allows_a_tool_inside_the_worktree() {
        let (_temp, _root, worktree) = layout();
        let payload = payload(
            &worktree,
            "Edit",
            serde_json::json!({"file_path": worktree.join("src/main.rs")}),
        );
        let mut out = Vec::new();
        evaluate(&mut Cursor::new(payload), &mut out).unwrap();
        // 許可時は何も書かないので、ツールは Claude 通常の許可フローで進む。
        assert!(out.is_empty());
    }

    #[test]
    fn a_reader_failure_is_denied_not_allowed() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken pipe"))
            }
        }
        let mut out = Vec::new();
        evaluate(&mut Failing, &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("guard payload could not be read"));
        assert!(written.contains("\"permissionDecision\":\"deny\""));
    }

    #[test]
    fn denies_when_the_payload_is_missing_fields_or_unparseable() {
        for payload in [
            r#"{"tool_input":{"file_path":"/repo/src/main.rs"}}"#,
            r#"{"cwd":"/repo/.usagi/sessions/work","tool_input":{"command":"ls"}}"#,
            "garbage",
        ] {
            assert!(deny_reason(payload).is_some());
        }
    }

    #[test]
    fn denies_a_relative_cwd_and_a_missing_tool_input() {
        // 相対 cwd は絶対でないため拒否。
        let relative = payload(Path::new("relative/dir"), "Read", serde_json::json!({}));
        assert!(deny_reason(&relative).unwrap().contains("no absolute cwd"));
        // tool_input が object でない（欠落）場合も拒否。
        let (_temp, _root, worktree) = layout();
        let no_input = serde_json::json!({"cwd": worktree, "tool_name": "Read"}).to_string();
        assert!(
            deny_reason(&no_input)
                .unwrap()
                .contains("no object tool_input")
        );
        // tool_name が空／欠落の場合も拒否。
        let empty_name = serde_json::json!({"cwd": worktree, "tool_name": ""}).to_string();
        assert!(deny_reason(&empty_name).unwrap().contains("no tool_name"));
    }

    #[test]
    fn denies_unknown_tools_and_uncanonicalizable_cwd() {
        let (_temp, _root, worktree) = layout();
        let unknown = payload(&worktree, "FutureMutator", serde_json::json!({}));
        assert!(deny_reason(&unknown).unwrap().contains("unknown tool"));
        let missing = payload(
            &worktree.join("missing"),
            "Read",
            serde_json::json!({"file_path": "/etc/hosts"}),
        );
        assert!(
            deny_reason(&missing)
                .unwrap()
                .contains("cannot be canonicalized")
        );
    }

    #[test]
    fn session_write_without_a_file_path_and_an_unresolvable_target_are_denied() {
        let (_temp, _root, worktree) = layout();
        // 書き込みツールなのに file_path が無い。
        let no_path = payload(&worktree, "Write", serde_json::json!({}));
        assert!(deny_reason(&no_path).unwrap().contains("has no file_path"));
    }

    #[test]
    fn session_allows_read_only_tools_and_wellformed_bash_but_denies_malformed_bash() {
        let (_temp, _root, worktree) = layout();
        for tool in [
            "Read",
            "Glob",
            "Grep",
            "WebFetch",
            "WebSearch",
            "Task",
            "Skill",
            "TodoWrite",
            "AskUserQuestion",
            "mcp__usagi__issue_get",
        ] {
            let allowed = payload(
                &worktree,
                tool,
                serde_json::json!({"file_path": "/etc/hosts"}),
            );
            assert_eq!(deny_reason(&allowed), None, "{tool} should be allowed");
        }
        for command in ["sh -c 'echo x > /tmp/sentinel'", "rm -f /tmp/sentinel"] {
            let payload = payload(&worktree, "Bash", serde_json::json!({"command": command}));
            assert_eq!(deny_reason(&payload), None, "sandbox handles {command}");
        }
        // command が欠落・空白のみの Bash はどちらも拒否する。
        for empty in [serde_json::json!({}), serde_json::json!({"command": "   "})] {
            let malformed = payload(&worktree, "Bash", empty);
            assert!(deny_reason(&malformed).unwrap().contains("has no command"));
        }
    }

    #[test]
    fn a_sessions_dir_without_a_usagi_grandparent_is_treated_as_root() {
        // `sessions` はあるが親が `.usagi` でない cwd は session worktree ではない → root モード。
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("notusagi/sessions/work");
        std::fs::create_dir_all(&cwd).unwrap();
        // root モードなので file 書き込みツールはパスによらず拒否される。
        let payload = payload(
            &cwd,
            "Write",
            serde_json::json!({"file_path": cwd.join("x.rs")}),
        );
        assert!(
            deny_reason(&payload)
                .unwrap()
                .contains("ワークスペースルート")
        );
    }

    #[test]
    fn root_mode_denies_a_write_tool_at_any_path() {
        // cwd が workspace root（`.usagi/sessions/` 配下でない）なので、repo 内の書き込みでも拒否。
        let (temp, _root, _worktree) = layout();
        let payload = payload(
            temp.path(),
            "Write",
            serde_json::json!({"file_path": temp.path().join("src/main.rs")}),
        );
        let mut out = Vec::new();
        evaluate(&mut Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("Write"));
    }

    #[test]
    fn root_mode_denies_a_mutating_git_command_and_malformed_bash() {
        let (temp, _root, _worktree) = layout();
        let mutating = payload(
            temp.path(),
            "Bash",
            serde_json::json!({"command": "git commit -m x"}),
        );
        assert!(deny_reason(&mutating).unwrap().contains("git commit -m x"));
        // command が欠落・空白のみの Bash はどちらも拒否する。
        for empty in [serde_json::json!({}), serde_json::json!({"command": "  "})] {
            let malformed = payload(temp.path(), "Bash", empty);
            assert!(deny_reason(&malformed).unwrap().contains("has no command"));
        }
    }

    #[test]
    fn root_mode_allows_read_only_git_and_other_tools() {
        let (temp, _root, _worktree) = layout();
        let git = payload(
            temp.path(),
            "Bash",
            serde_json::json!({"command": "git status"}),
        );
        assert_eq!(deny_reason(&git), None);
        let read = payload(
            temp.path(),
            "Read",
            serde_json::json!({"file_path": "/etc/hosts"}),
        );
        assert_eq!(deny_reason(&read), None);
        // mcp ツールも通す。
        let mcp = payload(
            temp.path(),
            "mcp__usagi__session_list",
            serde_json::json!({}),
        );
        assert_eq!(deny_reason(&mcp), None);
    }

    #[test]
    fn root_mode_denies_unknown_tools() {
        let (temp, _root, _worktree) = layout();
        let unknown = payload(temp.path(), "FutureMutator", serde_json::json!({}));
        assert!(deny_reason(&unknown).unwrap().contains("unknown tool"));
    }

    #[test]
    fn root_mode_denies_adversarial_shell_commands() {
        let (temp, _root, _worktree) = layout();
        for command in [
            "sh -c 'git commit -m x'",
            "git status > /tmp/sentinel",
            "sed -i s/a/b/ file",
            "rm -f file",
            "env git commit -m x",
            "/usr/bin/git commit -m x",
            "command git status",
        ] {
            let payload = payload(temp.path(), "Bash", serde_json::json!({"command": command}));
            assert!(deny_reason(&payload).is_some(), "allowed {command}");
        }
    }
}
