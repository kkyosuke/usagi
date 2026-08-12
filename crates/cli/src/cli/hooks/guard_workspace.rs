//! `usagi guard-workspace` — worktree の外へ出るツール呼び出しを拒否する内部コマンド。
//!
//! usagi がエージェント起動時に Claude の `PreToolUse` フックへ配線し、フックが JSON payload
//! （エージェントの `cwd`・`tool_name`・tool 入力）を stdin で渡して呼ぶ。人手で叩くものでは
//! ない（`--help` 非表示）。malformed・明白に不正な呼び出しを fail closed で拒否する。
//! これは多層防御の一層であり、hard boundary は将来 `claude-sandbox` が入れる OS sandbox が担う。
//!
//! 判定対象は**ツール名の allowlist ではなく変更能力**である（[`workspace_guard::classify_tool`]）。
//! harness は tool を追加・改名し続けるため、名前の closed allowlist は更新のたびに壊れ、guard が
//! 守るべき性質と無関係に agent の手足（`ToolSearch` 経由の MCP tool、subagent、task 追跡…）を
//! 奪ってしまう。未知でもファイルを名指しする input を持てば書き込みうるもの、command を運ぶ input を
//! 持てば shell として扱うので、`Bash` の改名や新しい実行系ツールでも判定は素通りしない。
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

/// `tool_input` の値のうち、`matches` に当たる key が運ぶ空でない文字列。書き込み先候補と
/// command 候補はどちらもこの形で拾い、1 つでも危険なら拒否する（配列や非文字列は拾えないため
/// 候補ゼロ＝fail-closed の拒否に落ちる）。
fn string_values(
    input: &serde_json::Map<String, serde_json::Value>,
    matches: fn(&str) -> bool,
) -> Vec<&str> {
    input
        .iter()
        .filter(|(key, _)| matches(key))
        .filter_map(|(_, value)| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .collect()
}

/// 書き込み先候補。`Write` の `file_path`、`NotebookEdit` の `notebook_path`、未知ツールの
/// `target_file` などを同じ経路で拾う。
fn path_targets(input: &serde_json::Map<String, serde_json::Value>) -> Vec<PathBuf> {
    string_values(input, workspace_guard::is_path_input_key)
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// session モード: file 書き込みの対象を canonicalize し、escape を弾く。非書き込みツールは
/// 通す。shell / subagent の副作用は（将来の）OS sandbox に閉じ込められる。
fn session_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
    worktree: &Path,
) -> Option<String> {
    match workspace_guard::classify_tool(tool_name, input.keys().map(String::as_str)) {
        workspace_guard::ToolGuard::FileWrite => {
            let targets = path_targets(input);
            if targets.is_empty() {
                return Some(format!(
                    "{tool_name} payload names no usable file path（書き込み先の key に空でない文字列がありません）"
                ));
            }
            // escape、または解決できないケースは fail-closed で拒否する（判定は core 側で total）。
            targets
                .into_iter()
                .find(|target| workspace_guard::path_escapes_root(worktree, cwd, target))
                .map(|target| {
                    format!(
                        "{} はセッション worktree {} の外です。",
                        target.display(),
                        worktree.display()
                    )
                })
        }
        // shell command と subagent は必須の OS sandbox を継承する。フックは shape を検証するが、
        // shell semantics を security boundary としてパースするとは主張しない。
        workspace_guard::ToolGuard::Shell => shell_commands(input)
            .is_empty()
            .then(|| format!("{tool_name} payload has no command")),
        workspace_guard::ToolGuard::Unrestricted => None,
    }
}

/// 検査対象の shell command 候補。`Bash` の `command` も、command を運ぶ未知ツールの key も
/// 同じ判定（[`workspace_guard::is_command_input_key`]）で拾う。
fn shell_commands(input: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
    string_values(input, workspace_guard::is_command_input_key)
}

/// root モード: コーディネータはリポジトリを変更してはならない。file 書き込みツールをパスによらず
/// 拒否し、厳格な read-only allowlist 外の shell command をすべて拒否する。
fn root_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    match workspace_guard::classify_tool(tool_name, input.keys().map(String::as_str)) {
        workspace_guard::ToolGuard::FileWrite => Some(format!(
            "ワークスペースルート（コーディネータ）ではファイルを書きうるツール（{tool_name}）を実行できません。\
             root 行はリポジトリを変更しません。編集はセッションの worktree に委譲してください。"
        )),
        workspace_guard::ToolGuard::Shell => {
            let commands = shell_commands(input);
            if commands.is_empty() {
                return Some(format!("{tool_name} payload has no command"));
            }
            // 1 つでも変更しうる command があれば拒否する。
            commands
                .into_iter()
                .find(|command| workspace_guard::command_mutates_repo(command))
                .map(|command| {
                    format!(
                        "ワークスペースルートでは read-only allowlist 外の shell command を実行できません（{command}）。\
                         Git は `git --no-pager --no-optional-locks <subcommand>` を使い、diff 系には \
                         `--no-ext-diff --no-textconv` も指定してください。"
                    )
                })
        }
        workspace_guard::ToolGuard::Unrestricted => None,
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
    fn session_confines_an_unknown_tool_that_names_a_file() {
        // 名前を知らないツールでも、file を名指しする input を持つなら書き込みうるとみなす。
        // worktree の外を指せば拒否、内側なら通す（harness が tool を増やしても壊れない）。
        let (_temp, root, worktree) = layout();
        let outside = payload(
            &worktree,
            "FutureMutator",
            serde_json::json!({"path": root.join("src/main.rs")}),
        );
        assert!(deny_reason(&outside).unwrap().contains("src/main.rs"));
        let inside = payload(
            &worktree,
            "FutureMutator",
            serde_json::json!({"notebook_path": worktree.join("nb.ipynb")}),
        );
        assert_eq!(deny_reason(&inside), None);
        // path も command も持たない未知のツールは変更能力の証拠がないので通す。
        let harmless = payload(
            &worktree,
            "FutureMutator",
            serde_json::json!({"query": "x"}),
        );
        assert_eq!(deny_reason(&harmless), None);
        // command を運ぶ未知のツールは shell として shape だけ検証する（副作用は OS sandbox）。
        let runner = payload(
            &worktree,
            "FutureRunner",
            serde_json::json!({"script": "rm -rf /tmp/x"}),
        );
        assert_eq!(deny_reason(&runner), None);
        let empty_runner = payload(&worktree, "FutureRunner", serde_json::json!({"cmd": "  "}));
        assert!(
            deny_reason(&empty_runner)
                .unwrap()
                .contains("has no command")
        );
    }

    #[test]
    fn denies_an_uncanonicalizable_cwd() {
        let (_temp, _root, worktree) = layout();
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
        // 書き込みツールなのに使える file_path が無い（欠落・空文字・文字列でない）。
        for input in [
            serde_json::json!({}),
            serde_json::json!({"file_path": ""}),
            serde_json::json!({"file_path": 7}),
        ] {
            let no_path = payload(&worktree, "Write", input);
            assert!(
                deny_reason(&no_path)
                    .unwrap()
                    .contains("names no usable file path")
            );
        }
        // `notebook_path` しか持たない `NotebookEdit` も同じ経路で解決される（旧実装は常に拒否した）。
        let notebook = payload(
            &worktree,
            "NotebookEdit",
            serde_json::json!({"notebook_path": worktree.join("nb.ipynb")}),
        );
        assert_eq!(deny_reason(&notebook), None);
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
            "Agent",
            "Skill",
            "ToolSearch",
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
            serde_json::json!({"command": "git --no-pager --no-optional-locks status"}),
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
    fn root_mode_denies_an_unknown_tool_that_names_a_file_but_allows_orchestration() {
        let (temp, _root, _worktree) = layout();
        // 未知でも file を名指しするツールは root では拒否する（root はリポジトリを変更しない）。
        let unknown_write = payload(
            temp.path(),
            "FutureMutator",
            serde_json::json!({"file_path": "/etc/hosts"}),
        );
        assert!(
            deny_reason(&unknown_write)
                .unwrap()
                .contains("FutureMutator")
        );
        // 委譲に必要な非書き込みツールは通る。`ToolSearch` が塞がれると deferred な
        // `mcp__usagi__*` の schema を取れず、root は session へ委譲する手段を失う。
        for tool in ["ToolSearch", "Agent", "TaskCreate", "EnterPlanMode"] {
            let allowed = payload(temp.path(), tool, serde_json::json!({"query": "usagi"}));
            assert_eq!(deny_reason(&allowed), None, "{tool} should be allowed");
        }
    }

    #[test]
    fn root_mode_checks_commands_from_an_unknown_runner_not_just_bash() {
        // `Bash` が改名されても root の read-only allowlist を素通りさせない。command を運ぶ
        // key を持つ未知のツールは shell として検査する。
        let (temp, _root, _worktree) = layout();
        let mutating = payload(
            temp.path(),
            "FutureRunner",
            serde_json::json!({"script": "git commit -m x"}),
        );
        assert!(deny_reason(&mutating).unwrap().contains("git commit -m x"));
        let read_only = payload(
            temp.path(),
            "FutureRunner",
            serde_json::json!({"cmd": "git --no-pager --no-optional-locks status"}),
        );
        assert_eq!(deny_reason(&read_only), None);
        // 変更しうる command が 1 つでもあれば拒否する。
        let mixed = payload(
            temp.path(),
            "FutureRunner",
            serde_json::json!({"cmd": "git --no-pager --no-optional-locks status", "script": "rm -rf x"}),
        );
        assert!(deny_reason(&mixed).unwrap().contains("rm -rf x"));
        // command key はあるが使える値が無い場合も拒否する。
        let empty = payload(temp.path(), "FutureRunner", serde_json::json!({"cmd": 7}));
        assert!(deny_reason(&empty).unwrap().contains("has no command"));
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
            "git -c diff.external=touch diff --ext-diff",
            "git --no-pager --no-optional-locks diff HEAD",
            "command git status",
        ] {
            let payload = payload(temp.path(), "Bash", serde_json::json!({"command": command}));
            assert!(deny_reason(&payload).is_some(), "allowed {command}");
        }
    }
}
