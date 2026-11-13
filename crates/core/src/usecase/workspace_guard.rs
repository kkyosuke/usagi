//! エージェントのツール呼び出しを「どこで動いているか」に応じて許可判定するロジック。
//!
//! usagi の session worktree はメインリポジトリの **内側**（`<repo>/.usagi/sessions/<name>/`）に
//! 置かれるため、リポジトリルートや別セッションの worktree がディスク上で 1 つ上の階層に並ぶ。
//! `<repo>/src/...` を編集したり親リポジトリへ `cd` するエージェントは、意図とは別のツリーを
//! 触ってしまう。この module はその判定を持ち、Claude Code の `PreToolUse` フックへどう配線するかは
//! [`guard-workspace`](../../../../usagi_cli/cli/hooks/guard_workspace) が持つ。session モードだけは、
//! symlink 経由の脱出を正しく拒否するため [`std::fs::canonicalize`] と path existence の read-only
//! filesystem 照会を行う。
//!
//! 判定はエージェントの `cwd` を起点に 2 モードに分かれる。
//!
//! - **session モード**（[`path_escapes_root`]）: エージェントは session worktree 内で動き、
//!   その配下なら何でも編集できる。存在する ancestor を canonicalize して symlink を解決し、
//!   字句上は worktree 内でも実体が外へ出る書き込みを弾く。新規作成のため、存在しない末尾
//!   component は許容する。
//! - **root モード**（[`is_write_tool`] / [`command_mutates_repo`]）: コーディネータが workspace
//!   root（CLI が canonical な cwd から session worktree を解決できない場合）で動く場合。
//!   リポジトリを一切変更してはならないため、パスによらず file 書き込みツールをすべて拒否し、
//!   曖昧さなく read-only な shell command だけを許可する。

use std::path::{Component, Path, PathBuf};

/// root モードで一律拒否する、ファイルへ書き込むツール。フック payload の `tool_name` に
/// 対して大文字小文字を区別して照合する。`Bash` はここに含めない（[`command_mutates_repo`]
/// が command 単位で検査し、read-only な shell / git は通す）。
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// リポジトリを変更しないと分かっているツール。ここに載るのは高速路であって安全性の根拠では
/// なく、載っていない未知のツールは [`classify_tool`] の input shape で判定される。
const NON_MUTATING_TOOLS: &[&str] = &[
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
    "Monitor",
];

/// `tool_input` の key がファイルを名指しすると判断する末尾の語。key は完全一致ではなく
/// `snake_case` に正規化した最後の segment で照合するため、`file_path` / `notebook_path` /
/// `target_file` / `output_dir` / camelCase の `filePath` / 複数形の `paths` がすべて当たり、
/// `profile` のように語の一部が偶然一致するだけの key は当たらない。
const PATH_KEY_WORDS: &[&str] = &[
    "path",
    "file",
    "filename",
    "dir",
    "directory",
    "destination",
];

/// `tool_input` の key が shell command を運ぶと判断する末尾の語。[`PATH_KEY_WORDS`] と同じく
/// 正規化した最後の segment で照合する。
const COMMAND_KEY_WORDS: &[&str] = &["command", "cmd", "script", "shell"];

/// `tool_name` が root モードで一律拒否される file 書き込みツールかどうか。
#[must_use]
pub fn is_write_tool(tool_name: &str) -> bool {
    WRITE_TOOLS.contains(&tool_name)
}

/// key を `snake_case` へ正規化し、最後の segment を返す（複数形の `s` は落とす）。
/// `filePath` → `path`、`file_paths` → `path`、`profile` → `profile`。
fn key_word(key: &str) -> String {
    let mut snake = String::with_capacity(key.len() + 4);
    for character in key.chars() {
        if character.is_ascii_uppercase() && !snake.is_empty() {
            snake.push('_');
        }
        snake.push(character.to_ascii_lowercase());
    }
    let last = snake.rsplit(['_', '-']).next().unwrap_or_default();
    last.strip_suffix('s').unwrap_or(last).to_string()
}

/// `tool_input` の key がファイルを名指しするか。guard のシムが書き込み先候補を拾うために使う。
#[must_use]
pub fn is_path_input_key(key: &str) -> bool {
    PATH_KEY_WORDS.contains(&key_word(key).as_str())
}

/// `tool_input` の key が shell command を運ぶか。guard のシムが検査対象の command を拾うために使う。
#[must_use]
pub fn is_command_input_key(key: &str) -> bool {
    COMMAND_KEY_WORDS.contains(&key_word(key).as_str())
}

/// guard がツール呼び出しに課す制限の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGuard {
    /// ファイルを書きうる。session モードでは書き込み先を worktree 内へ閉じ込め、root モードでは
    /// パスによらず拒否する。
    FileWrite,
    /// shell command。command 単位で [`command_mutates_repo`] が検査する。
    Shell,
    /// リポジトリを変更しない。両モードとも通す。
    Unrestricted,
}

/// ツール呼び出しの分類。名前と `tool_input` の key だけで決まるため、JSON 語彙を持たない
/// usecase 層に閉じている（シムが key を渡す）。
///
/// 判定は「未知の名前を拒否する」closed allowlist ではなく、**変更能力を持つ shape** を捕まえる。
/// harness は tool を追加・改名し続けるため（`Task` → `Agent`、`TodoWrite` → task 系、`ToolSearch`
/// の追加…）、名前の allowlist は更新のたびに壊れて agent の手足を無関係に奪う。guard が守る
/// 性質（worktree 外への書き込みと root の repository mutation を止める）は書き込み先の閉じ込めと
/// command の検査が担っており、名前を知らないことは変更能力の証拠ではない。
///
/// したがって未知のツールは shape で倒す。file を名指しする key（[`is_path_input_key`]）を持てば
/// [`ToolGuard::FileWrite`]、command を運ぶ key（[`is_command_input_key`]）を持てば
/// [`ToolGuard::Shell`] として扱い、どちらも持たないものだけを通す。`Bash` が改名されたり別の
/// 実行系ツールが増えたりしても、root の read-only shell allowlist が素通りされない。
#[must_use]
pub fn classify_tool<'a>(
    tool_name: &str,
    input_keys: impl IntoIterator<Item = &'a str>,
) -> ToolGuard {
    if is_write_tool(tool_name) {
        return ToolGuard::FileWrite;
    }
    if tool_name == "Bash" {
        return ToolGuard::Shell;
    }
    // MCP tool は user が明示的に配線した server の面であり、usagi 自身の server は root からの
    // tracked store 書き込みを自前で拒否する。
    if tool_name.starts_with("mcp__") || NON_MUTATING_TOOLS.contains(&tool_name) {
        return ToolGuard::Unrestricted;
    }
    // 書き込み先の閉じ込めのほうが強い制約なので、両方の shape を持つ呼び出しは FileWrite に倒す。
    let mut shell = false;
    for key in input_keys {
        if is_path_input_key(key) {
            return ToolGuard::FileWrite;
        }
        shell |= is_command_input_key(key);
    }
    if shell {
        return ToolGuard::Shell;
    }
    ToolGuard::Unrestricted
}

/// リポジトリを読むだけの git サブコマンド。root モードで許可する。これ以外で `git` に届く
/// ものは変更の可能性ありとして拒否する。曖昧・未知のサブコマンド（`config` / `branch` /
/// `remote` など）は許可せず塞ぐ側に倒す（allow-list が fail-safe）。コーディネータが
/// メインリポジトリに対してこれらを走らせる必要もない。
const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "reflog",
    "shortlog",
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "show-ref",
    "name-rev",
    "merge-base",
    "whatchanged",
    "grep",
    "cherry",
    "diff-tree",
    "diff-index",
    "diff-files",
    "for-each-ref",
    "count-objects",
    "version",
];

/// Commands whose output can enter Git's diff machinery. They are allowed only
/// when both external diff and textconv are explicitly disabled.
const DIFF_CAPABLE_GIT_SUBCOMMANDS: &[&str] = &[
    "log",
    "diff",
    "show",
    "blame",
    "reflog",
    "whatchanged",
    "diff-tree",
    "diff-index",
    "diff-files",
];

/// root モードの `Bash` command がリポジトリ／ファイルシステムを変更しうるかどうか。
/// 厳格な read-only allowlist から外れるものはすべて変更ありとみなす。
#[must_use]
pub fn command_mutates_repo(command: &str) -> bool {
    !root_command_is_read_only(command)
}

/// root コーディネータの shell command が曖昧さなく read-only かどうか。
///
/// これは意図的に小さな allowlist であってシェルパーサではない。シェル構文・ラッパー・
/// インタプリタ・リダイレクト・コマンド置換・変更を伴うユーティリティ・未知の実行ファイルは
/// すべて拒否する。任意 path に置かれた同名 executable を許可しないため、program は bare name
/// だけを受け付け、daemon が固定した `PATH` で解決させる。
#[must_use]
pub fn root_command_is_read_only(command: &str) -> bool {
    if command.trim().is_empty()
        || command
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | ';' | '&' | '>' | '<' | '`' | '$'))
    {
        return false;
    }
    // A read-only producer may be narrowed by another read-only command. Keep
    // the supported composition deliberately small: no `||`, redirection,
    // substitutions, or multiline shell syntax.
    if command.contains('|') {
        return !command.contains("||")
            && command
                .split('|')
                .all(|segment| root_simple_command_is_read_only(segment.trim()));
    }
    root_simple_command_is_read_only(command)
}

fn root_simple_command_is_read_only(command: &str) -> bool {
    // クォート不整合などで字句分割に失敗したら空トークン列に倒し、先頭語が取れない
    // （分割失敗を含む）場合は read-only と判定しない。
    let tokens = shell_words::split(command).unwrap_or_default();
    let Some(program) = tokens.first() else {
        return false;
    };
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if program == "git" {
        return git_command_is_read_only(&tokens[1..]);
    }
    if program != basename {
        return false;
    }
    if basename == "gh" {
        return matches!(tokens.get(1).map(String::as_str), Some("pr"))
            && matches!(tokens.get(2).map(String::as_str), Some("view" | "list"))
            && !gh_opens_browser(&tokens);
    }
    if basename == "sleep" {
        if tokens.len() != 2 {
            return false;
        }
        let (number, multiplier) = tokens[1].strip_suffix('s').map_or_else(
            || {
                tokens[1]
                    .strip_suffix('m')
                    .map_or((tokens[1].as_str(), 1_u64), |number| (number, 60))
            },
            |number| (number, 1),
        );
        return number
            .parse::<u64>()
            .ok()
            .and_then(|number| number.checked_mul(multiplier))
            .is_some_and(|seconds| seconds <= 300);
    }
    matches!(
        basename,
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "stat" | "test" | "true" | "false"
    )
}

fn short_flag_group(token: &str) -> Option<&str> {
    token
        .strip_prefix('-')
        .filter(|flags| !flags.is_empty() && !flags.starts_with('-'))
}

fn git_command_is_read_only(git_tokens: &[String]) -> bool {
    let Some((subcommand_index, subcommand)) = git_subcommand_from_tokens(git_tokens) else {
        return false;
    };
    let subcommand_args = &git_tokens[subcommand_index + 1..];
    let unsafe_option = subcommand_args.iter().any(|token| {
        token.starts_with("-c")
            || token.starts_with("--config-env")
            || token.starts_with("-C")
            || token.starts_with("--git-dir")
            || token.starts_with("--work-tree")
            || token.starts_with("-o")
            || token.starts_with("--output")
            || token.starts_with("--upload-pack")
            || token.starts_with("--exec")
            || token.starts_with("--open")
            || token.starts_with("-O")
            || (subcommand == "grep"
                && short_flag_group(token).is_some_and(|flags| flags.contains('O')))
            || (subcommand == "status"
                && (token.starts_with("--verb")
                    || token == "--long"
                    || short_flag_group(token).is_some_and(|flags| flags.contains('v'))))
            || token.starts_with("--paginate")
            || token.starts_with("--ext-d")
            || token.starts_with("--textc")
            || token.starts_with("--show-sig")
    });
    let reflog_mutates = subcommand == "reflog"
        && subcommand_args
            .iter()
            .find(|token| !token.starts_with('-'))
            .is_some_and(|action| {
                matches!(action.as_str(), "delete" | "expire" | "exists" | "drop")
            });
    let log_requests_diff = subcommand == "log"
        && subcommand_args.iter().any(|token| {
            token.starts_with("--patch")
                || token.starts_with("--stat")
                || token == "--numstat"
                || token == "--shortstat"
                || token == "--raw"
                || token.starts_with("--name-")
                || token == "--cc"
                || token.starts_with("--remerge")
                || token.starts_with("--diff-merges")
                || token.starts_with("--unified")
                || token.starts_with("--dirstat")
                || token.starts_with("--word-diff")
                || token.starts_with("--color-words")
                || token == "--full-diff"
                || token == "--summary"
                || token == "--check"
                || token.starts_with("--find-renames")
                || token.starts_with("--find-copies")
                || short_flag_group(token).is_some_and(|flags| {
                    flags.chars().any(|flag| {
                        matches!(
                            flag,
                            'p' | 'u' | 'U' | 'm' | 'S' | 'G' | 'L' | 'B' | 'M' | 'C' | 'D' | 'X'
                        )
                    })
                })
        });
    let diff_helpers_disabled = !DIFF_CAPABLE_GIT_SUBCOMMANDS.contains(&subcommand)
        || (subcommand == "log" && !log_requests_diff)
        || (subcommand_args.iter().any(|token| token == "--no-ext-diff")
            && subcommand_args.iter().any(|token| token == "--no-textconv"));
    READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand)
        && !unsafe_option
        && !reflog_mutates
        && diff_helpers_disabled
}

/// Match the browser flag as gh parses it, including grouped short flags.
/// Argument-taking flags end a short group and consume their following value.
fn gh_opens_browser(tokens: &[String]) -> bool {
    let mut arguments = tokens.iter().skip(3);
    while let Some(token) = arguments.next() {
        if token == "--" {
            break;
        }
        if token == "--web" || token.starts_with("--web=") {
            return true;
        }
        if matches!(
            token.as_str(),
            "--repo"
                | "--jq"
                | "--template"
                | "--assignee"
                | "--author"
                | "--base"
                | "--head"
                | "--label"
                | "--limit"
                | "--search"
                | "--state"
        ) {
            arguments.next();
            continue;
        }
        let Some(flags) = short_flag_group(token) else {
            continue;
        };
        for (offset, flag) in flags.char_indices() {
            if flag == 'w' {
                return true;
            }
            if "RqtAaBHLlSs".contains(flag) {
                if offset + flag.len_utf8() == flags.len() {
                    arguments.next();
                }
                break;
            }
        }
    }
    false
}

/// サブコマンドより前の git グローバルオプションは、副作用や未知の値消費を持たない最小限の
/// allowlist だけを受け付ける。未知のグローバルオプション（`--attr-source <tree>` 等）は値トークンを
/// サブコマンドと誤認させるため fail-closed で拒否する。
fn git_subcommand_from_tokens(tokens: &[String]) -> Option<(usize, &str)> {
    let mut index = 0;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--no-pager"
            | "-P"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "-C."
            | "-C=." => {
                index += 1;
            }
            "-C" => {
                if tokens.get(index + 1).map(String::as_str) != Some(".") {
                    return None;
                }
                index += 2;
            }
            token if token.starts_with('-') => return None,
            subcommand => return Some((index, subcommand)),
        }
    }
    None
}

/// ツールの対象パスが `root`（session worktree）の外へ解決されるかどうか。true のとき呼び出し側は
/// 拒否する。新規ファイルのため末尾の存在しない component は許容し、`symlink_metadata` で実在する
/// ancestor（リンク先未作成の dangling symlink を含む）まで解決してから字句的に付け直す。
/// `root` / `cwd` を canonicalize できない、dangling symlink を踏む、または cwd が root の外にある
/// ケースは、安全のため escape 扱い（true）にする（fail-closed）。
#[must_use]
pub fn path_escapes_root(root: &Path, cwd: &Path, target: &Path) -> bool {
    let (Ok(root), Ok(cwd)) = (std::fs::canonicalize(root), std::fs::canonicalize(cwd)) else {
        return true;
    };
    if !cwd.starts_with(&root) {
        return true;
    }
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd.join(target)
    };
    // Preserve symlink/.. ordering on the raw path first, then re-verify the
    // normalized path so `missing/../escape_symlink/file` cannot expose an
    // unchecked symlink after lexical `..` folding.
    let Some(first) = resolve_against_existing_ancestor(&absolute) else {
        return true;
    };
    if !first.starts_with(&root) {
        return true;
    }
    first != absolute
        && resolve_against_existing_ancestor(&first).is_none_or(|second| !second.starts_with(&root))
}

fn resolve_against_existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.symlink_metadata().is_ok())
        .and_then(|existing| {
            let mut resolved = std::fs::canonicalize(existing).ok()?;
            resolved.push(path.strip_prefix(existing).unwrap_or(Path::new("")));
            Some(normalize(&resolved))
        })
}

/// `path` から `.` と `..` を字句的に畳み込む（ファイルシステムを参照しない）。`..` は直前に
/// 残した component を pop する（root では no-op）ため、結果がそのパス自身の root より上へ
/// 登ることはない。まだ作成されていないファイルも解決できるよう [`std::fs::canonicalize`] の
/// 代わりに使う。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_tools_and_shell_are_classified_by_name() {
        for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
            assert!(is_write_tool(tool));
            assert_eq!(classify_tool(tool, []), ToolGuard::FileWrite);
        }
        assert!(!is_write_tool("Bash"));
        assert_eq!(classify_tool("Bash", ["command"]), ToolGuard::Shell);
    }

    #[test]
    fn known_non_mutating_tools_pass_even_when_they_name_a_file() {
        // `Read` / `Grep` は path を取るが読むだけなので、worktree の外でも通す。
        for tool in ["Read", "Glob", "Grep", "ToolSearch", "Agent", "Monitor"] {
            assert_eq!(
                classify_tool(tool, ["file_path", "path"]),
                ToolGuard::Unrestricted,
                "{tool}"
            );
        }
        // MCP tool は配線した server の面としてそのまま通す。
        assert_eq!(
            classify_tool("mcp__usagi__memory_get", ["name"]),
            ToolGuard::Unrestricted
        );
    }

    #[test]
    fn an_unknown_tool_is_classified_by_its_input_shape() {
        // 名前を知らないだけでは変更能力の証拠にならない（harness は tool を増やし続ける）。
        assert_eq!(
            classify_tool("FutureReader", ["query", "max_results"]),
            ToolGuard::Unrestricted
        );
        // ファイルを名指しするなら書き込みうるとみなす（fail-closed）。
        for key in [
            "file_path",
            "notebook_path",
            "path",
            "paths",
            "target_file",
            "output_dir",
            "filePath",
            "destination",
        ] {
            assert!(is_path_input_key(key), "{key}");
            assert_eq!(
                classify_tool("FutureMutator", [key]),
                ToolGuard::FileWrite,
                "{key}"
            );
        }
        // command を運ぶなら shell として検査する。`Bash` が改名されても root の allowlist が残る。
        for key in ["command", "cmd", "script", "shell_command", "commands"] {
            assert!(is_command_input_key(key), "{key}");
            assert_eq!(
                classify_tool("FutureRunner", [key]),
                ToolGuard::Shell,
                "{key}"
            );
        }
        // 両方持つなら、より強い制約である書き込み先の閉じ込めに倒す。
        assert_eq!(
            classify_tool("FutureBoth", ["command", "file_path"]),
            ToolGuard::FileWrite
        );
        // 語の一部が偶然一致するだけの key は当たらない（`profile` は `file` ではない）。
        for key in ["query", "profile", "max_results", "prompt", "description"] {
            assert!(!is_path_input_key(key), "{key}");
            assert!(!is_command_input_key(key), "{key}");
        }
    }

    #[test]
    fn an_empty_key_normalizes_without_panicking() {
        // 空の key は語を持たないため、どちらの shape にも当たらない。
        assert!(!is_path_input_key(""));
        assert!(!is_command_input_key(""));
    }

    #[test]
    fn canonical_paths_preserve_component_boundaries_and_relative_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        let sibling = temp.path().join("work2");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        for target in [
            root.clone(),
            root.join("src/new.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/../Cargo.toml"),
        ] {
            assert!(!path_escapes_root(&root, &root, &target), "{target:?}");
        }
        for target in [
            temp.path().join("outside.rs"),
            sibling.join("src/main.rs"),
            PathBuf::from("../outside.rs"),
        ] {
            assert!(path_escapes_root(&root, &root, &target), "{target:?}");
        }
        assert_eq!(normalize(Path::new("/../../")), PathBuf::from("/"));
    }

    #[test]
    fn normalize_drops_a_leading_current_dir_component() {
        // 先頭の `.` は `Path::components` が唯一保持する `CurDir` 形（途中の `.` は既に畳まれる）。
        // 直接正規化すると skip され、実 component だけが残る。
        assert_eq!(normalize(Path::new("./a/b")), PathBuf::from("a/b"));
    }

    #[test]
    fn write_tools_are_recognized_case_sensitively() {
        for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
            assert!(is_write_tool(tool), "{tool} should be a write tool");
        }
        for tool in ["Read", "Grep", "Glob", "Bash", "Task", "write"] {
            assert!(!is_write_tool(tool), "{tool} should not be a write tool");
        }
    }

    #[test]
    fn mutating_git_commands_are_flagged() {
        for command in [
            "git commit -m 'x'",
            "git add .",
            "git push",
            "git merge main",
            "git rebase main",
            "git checkout -b feat/x",
            "git worktree add ../wt",
            "git reset --hard",
            "git config user.name x",
            "git branch -D old",
        ] {
            assert!(
                command_mutates_repo(command),
                "{command} should be flagged as mutating"
            );
        }
    }

    #[test]
    fn read_only_git_commands_are_allowed() {
        for command in [
            "git --no-pager --no-optional-locks status",
            "git --no-pager --no-optional-locks log --no-ext-diff --no-textconv --oneline",
            "git --no-pager --no-optional-locks diff --no-ext-diff --no-textconv HEAD~1",
            "git --no-pager --no-optional-locks show --no-ext-diff --no-textconv abc123",
            "git --no-pager --no-optional-locks rev-parse HEAD",
            "git --no-pager --no-optional-locks ls-files",
        ] {
            assert!(
                !command_mutates_repo(command),
                "{command} should be allowed"
            );
        }
    }

    #[test]
    fn only_allowlisted_non_git_commands_are_not_flagged() {
        assert!(!command_mutates_repo("ls -la"));
        for command in ["cargo test", "echo hi", ""] {
            assert!(command_mutates_repo(command));
        }
    }

    #[test]
    fn a_mutating_git_anywhere_in_a_chain_is_flagged() {
        // 先頭が read-only な git でも、後続の mutating な git を見逃さない。
        assert!(command_mutates_repo("git status && git commit -m x"));
        assert!(command_mutates_repo("cd foo; git push"));
        assert!(command_mutates_repo("git log | cat && git add ."));
        // 見た目 read-only でも shell 合成そのものを拒否する。
        assert!(command_mutates_repo("git status && git log"));
    }

    #[test]
    fn global_options_before_the_subcommand_do_not_hide_it() {
        // `-C <path>` は値トークンを消費するため mutation のサブコマンドを見失わない。
        assert!(command_mutates_repo("git -C /repo commit -m x"));
        assert!(command_mutates_repo("git -c user.name=x commit"));
        // Root policy は repository/config scope の差し替え自体も拒否する。
        assert!(command_mutates_repo("git -C /repo status"));
    }

    #[test]
    fn wrappers_and_env_assignments_are_denied() {
        assert!(command_mutates_repo("sudo git push"));
        assert!(command_mutates_repo("GIT_DIR=/x git commit"));
        assert!(command_mutates_repo("env git rebase main"));
        assert!(command_mutates_repo("FOO=bar git status"));
    }

    #[test]
    fn git_with_no_subcommand_is_denied() {
        assert!(command_mutates_repo("git"));
        assert!(command_mutates_repo("git -C /repo"));
    }

    #[test]
    fn an_env_assignment_name_may_contain_digits_after_the_first_char() {
        // 名前は index 0 より後に数字を許す（`A1=…`）。そんな先頭代入があっても、
        // 背後の mutating な git は見通される。
        assert!(command_mutates_repo("A1=x git commit"));
    }

    #[test]
    fn root_shell_allowlist_rejects_wrappers_redirection_and_mutators() {
        for command in [
            "sh -c 'git status'",
            "git status > out",
            "sed -i s/a/b/ file",
            "rm file",
            "env git status",
            "command git status",
            "/usr/bin/git commit -m x",
            "/tmp/git --no-pager --no-optional-locks status",
            "rg --pre=touch needle",
            "git log --output=/tmp/out",
            "git log -o/tmp/out",
            "git -c diff.external=touch diff --ext-diff",
            "git -ccore.pager=touch status",
            "git -C.. status",
            "git -C=.. status",
            "git --config-env=diff.external=PWN git diff --ext-diff",
            "git --no-pager --no-optional-locks diff HEAD",
            "git --no-pager --no-optional-locks ls-remote --upload-pack=touch origin",
            "git --no-pager --no-optional-locks -p status",
            "git --no-pager --no-optional-locks show --no-ext-diff --no-textconv --show-signature",
            "git --no-pager --no-optional-locks grep -Otouch needle",
            "git reflog --no-ext-diff --no-textconv delete HEAD@{0}",
            "git reflog --no-ext-diff --no-textconv expire --expire=now --all",
            "git reflog --no-ext-diff --no-textconv exists refs/heads/main",
            "git reflog --no-ext-diff --no-textconv drop refs/heads/main",
            "git log -u main",
            "git log --patch-with-stat main",
            "git log --patch-with-raw main",
            "git log --cc main",
            "git log -m main",
            "git log -Sneedle main",
            "git log -Gneedle main",
            "git log -L1,5:src/main.rs",
            "echo $(git status)",
        ] {
            assert!(!root_command_is_read_only(command), "allowed {command}");
        }
        for command in [
            "git --no-pager --no-optional-locks status",
            "git --no-pager --no-optional-locks log --no-ext-diff --no-textconv --oneline",
            "git --no-pager --no-optional-locks diff --no-ext-diff --no-textconv",
            "git log --oneline main",
            "git log --stat --no-ext-diff --no-textconv main",
            "git log -Sneedle --no-ext-diff --no-textconv main",
            "git reflog --no-ext-diff --no-textconv",
            "git reflog --no-ext-diff --no-textconv show HEAD",
            "git -C . status --short --branch",
            "git -C. status --short --branch",
            "git -C=. status --short --branch",
            "git log main | head -20",
            "gh pr view --comments",
            "gh pr list --limit 10",
            "sleep 1",
            "sleep 1s",
            "ls -la",
        ] {
            assert!(root_command_is_read_only(command), "denied {command}");
        }
        for command in [
            "git -C .. status",
            "git status | sh",
            "gh pr checkout 1",
            "gh pr view 1 --web",
            "git log --stat main",
            "sleep 1 2",
            "sleep forever",
            "sleep 6m",
        ] {
            assert!(!root_command_is_read_only(command), "allowed {command}");
        }
    }

    #[test]
    fn an_unparseable_shell_command_is_not_read_only() {
        // クォート不整合で字句分割に失敗 → 空トークン列 → 先頭語が取れず read-only ではない。
        assert!(!root_command_is_read_only("git status 'unbalanced"));
        assert!(command_mutates_repo("git status 'unbalanced"));
    }

    #[test]
    fn adversarial_command_variations_stay_denied() {
        let seeds = [
            "sh -c 'touch /tmp/x'",
            "git status > /tmp/x",
            "sed -i s/a/b/ file",
            "rm -rf target",
            "env git commit -m x",
        ];
        for seed in seeds {
            for command in [seed.to_string(), format!("  {seed}"), format!("{seed}  ")] {
                assert!(!root_command_is_read_only(&command), "allowed {command:?}");
            }
        }
    }

    #[test]
    fn canonical_path_check_blocks_a_real_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo/.usagi/sessions/work");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "safe").unwrap();
        let link = worktree.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, &link).unwrap();

        assert!(path_escapes_root(
            &worktree,
            &worktree,
            &link.join("sentinel")
        ));
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "safe");
        assert!(!path_escapes_root(
            &worktree,
            &worktree,
            Path::new("new/file")
        ));
    }

    #[test]
    fn gh_browser_aliases_are_denied_without_confusing_argument_values() {
        for command in [
            "gh pr view 1 --web",
            "gh pr view 1 --web=true",
            "gh pr view 1 --web=false",
            "gh pr view 1 -w",
            "gh pr view 1 -wc",
            "gh pr view 1 -cw",
            "gh pr view 1 -w=true",
            "gh pr list -w",
        ] {
            assert!(!root_command_is_read_only(command), "allowed {command}");
        }
        for command in [
            "gh pr view 1 -Rowner/web",
            "gh pr view 1 -R owner/web",
            "gh pr view 1 -ctw",
            "gh pr view 1 -t -w",
            "gh pr view 1 --template -w",
            "gh pr view 1 --help",
            "gh pr list --search web",
            "gh pr list -- -w",
        ] {
            assert!(root_command_is_read_only(command), "denied {command}");
        }
    }

    #[test]
    fn symlink_parent_components_follow_filesystem_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root.join("inside/child")).unwrap();
        std::fs::create_dir_all(outside.join("child")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("child"), root.join("escape")).unwrap();
            std::os::unix::fs::symlink(root.join("inside/child"), root.join("safe")).unwrap();
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(outside.join("child"), root.join("escape")).unwrap();
            std::os::windows::fs::symlink_dir(root.join("inside/child"), root.join("safe"))
                .unwrap();
        }
        for target in [
            Path::new("escape/../victim"),
            Path::new("escape/../new/file"),
        ] {
            assert!(path_escapes_root(&root, &root, target));
            assert!(path_escapes_root(&root, &root, &root.join(target)));
        }
        std::fs::write(outside.join("victim"), "safe").unwrap();
        assert!(path_escapes_root(
            &root,
            &root,
            Path::new("escape/../victim")
        ));
        assert!(!path_escapes_root(
            &root,
            &root,
            Path::new("safe/../new/file")
        ));
        assert!(!path_escapes_root(
            &root,
            &root,
            Path::new("inside/../new/file")
        ));
        assert_eq!(
            std::fs::read_to_string(outside.join("victim")).unwrap(),
            "safe"
        );
    }

    #[test]
    fn an_unresolvable_root_or_cwd_is_fail_closed_as_an_escape() {
        let temp = tempfile::tempdir().unwrap();
        // root を canonicalize できない → fail-closed で escape 扱い。
        let missing_root = temp.path().join("missing-root");
        assert!(path_escapes_root(
            &missing_root,
            temp.path(),
            Path::new("file")
        ));
        // cwd を canonicalize できない → 同上。
        let missing_cwd = temp.path().join("missing-cwd");
        assert!(path_escapes_root(
            temp.path(),
            &missing_cwd,
            Path::new("file")
        ));
    }

    #[test]
    fn a_cwd_outside_the_root_escapes() {
        // cwd が root の外なら、対象パスによらず escape。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(path_escapes_root(&root, &elsewhere, Path::new("file")));
    }

    #[test]
    fn git_global_flag_smuggling_and_diff_or_pager_bypasses_are_denied() {
        for command in [
            "git --attr-source status reset --hard",
            "git --list-cmds=main",
            "git -C",
            "git -C /tmp status",
            "git grep --open=sh needle",
            "git grep --open needle",
            "git grep --open-files=sh needle",
            "git grep -nOsh needle",
            "git grep --textconv needle",
            "git grep --textc needle",
            "git status -v",
            "git status -sv",
            "git status --verbose",
            "git status --verb",
            "git status --long",
            "git log -U0 main",
            "git log --unified=0 main",
            "git log -1p main",
            "git log -c main",
            "git log --remerge-diff main",
            "git log --diff-merges=on main",
            "git log --dirstat main",
            "git log --word-diff main",
            "git log --color-words main",
            "git log --full-diff main",
            "git log --summary main",
            "git log --check main",
            "git log --find-renames main",
            "git log --find-copies main",
            "git log --numstat main",
            "git log --shortstat main",
            "git log --raw main",
            "git log --name-only main",
            "git show --no-ext-diff --no-textconv --show-sig HEAD",
            "git diff --ext-d HEAD",
            "git diff --no-ext-diff --no-textconv -C /tmp HEAD",
            "git diff --no-ext-diff --no-textconv --git-dir=/tmp HEAD",
            "git diff --no-ext-diff --no-textconv --work-tree=/tmp HEAD",
            "git diff --no-ext-diff --no-textconv --exec-path=/tmp HEAD",
            "git diff --no-ext-diff --no-textconv --config-env=core.pager=ENV HEAD",
            "git diff --no-ext-diff --no-textconv --paginate HEAD",
            "git diff --no-ext-diff --no-textconv --exec=sh HEAD",
        ] {
            assert!(!root_command_is_read_only(command), "allowed {command}");
        }
        for command in [
            "git -P --literal-pathspecs --glob-pathspecs --noglob-pathspecs --icase-pathspecs status",
            "git log -U0 --no-ext-diff --no-textconv main",
            "git log --remerge-diff --no-ext-diff --no-textconv main",
            "git log -n 5 main",
            "git grep -n needle -- -Osh",
        ] {
            // Note: `-Osh` is denied even after `--` because subcommand_args is fail-closed;
            // verify only the safe forms pass.
            if command.contains("-Osh") {
                assert!(!root_command_is_read_only(command));
            } else {
                assert!(root_command_is_read_only(command), "denied {command}");
            }
        }
    }

    #[test]
    fn dangling_and_post_collapse_symlinks_cannot_escape_worktree_root() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo/.usagi/sessions/work");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let missing_outside = temp.path().join("not-created-yet");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&missing_outside, worktree.join("dangling")).unwrap();
            std::os::unix::fs::symlink(&outside, worktree.join("escape")).unwrap();
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&missing_outside, worktree.join("dangling")).unwrap();
            std::os::windows::fs::symlink_dir(&outside, worktree.join("escape")).unwrap();
        }
        assert!(path_escapes_root(
            &worktree,
            &worktree,
            Path::new("dangling")
        ));
        assert!(path_escapes_root(
            &worktree,
            &worktree,
            Path::new("dangling/new.txt")
        ));
        assert!(path_escapes_root(
            &worktree,
            &worktree,
            Path::new("missing/../escape/sentinel")
        ));
        assert!(path_escapes_root(
            &worktree,
            &worktree,
            Path::new("missing/../dangling/sentinel")
        ));
    }
}
