//! エージェントのツール呼び出しを「どこで動いているか」に応じて許可判定する純粋ロジック。
//!
//! usagi の session worktree はメインリポジトリの **内側**（`<repo>/.usagi/sessions/<name>/`）に
//! 置かれるため、リポジトリルートや別セッションの worktree がディスク上で 1 つ上の階層に並ぶ。
//! `<repo>/src/...` を編集したり親リポジトリへ `cd` するエージェントは、意図とは別のツリーを
//! 触ってしまう。この module はその判定の「純粋な決定部」で、Claude Code の `PreToolUse` フックへ
//! どう配線するかは [`guard-workspace`](../../../../usagi_cli/cli/hooks/guard_workspace) が持つ。
//!
//! 判定はエージェントの `cwd` を起点に 2 モードに分かれる。
//!
//! - **session モード**（[`path_escapes_root`]）: エージェントは session worktree 内で動き、
//!   その配下なら何でも編集できる。存在する ancestor を canonicalize して symlink を解決し、
//!   字句上は worktree 内でも実体が外へ出る書き込みを弾く。新規作成のため、存在しない末尾
//!   component は許容する。
//! - **root モード**（[`is_write_tool`] / [`command_mutates_repo`]）: コーディネータが workspace
//!   root（cwd が `.usagi/sessions/` 配下でない。[`is_session_worktree`] 参照）で動く場合。
//!   リポジトリを一切変更してはならないため、パスによらず file 書き込みツールをすべて拒否し、
//!   曖昧さなく read-only な shell command だけを許可する。

use std::path::{Component, Path, PathBuf};

/// `cwd` が usagi の session worktree（`<repo>/.usagi/sessions/<name>/…`）の内側かどうか。
///
/// コーディネータが動く workspace root とは区別する。pre-commit フックの免除
/// （`.usagi/sessions/` セグメントで判定。[06-conventions.md](../../../../document/06-conventions.md)）と
/// 同じ軸で、連続する `.usagi` → `sessions` component を含むパスを session worktree とみなす。
/// guard はこれで session モードと（より厳しい）root モードを選ぶ。
#[must_use]
pub fn is_session_worktree(cwd: &Path) -> bool {
    let names: Vec<&std::ffi::OsStr> = cwd
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    names
        .windows(2)
        .any(|pair| pair[0] == ".usagi" && pair[1] == "sessions")
}

/// root モードで一律拒否する、ファイルへ書き込むツール。フック payload の `tool_name` に
/// 対して大文字小文字を区別して照合する。`Bash` はここに含めない（[`command_mutates_repo`]
/// が command 単位で検査し、read-only な shell / git は通す）。
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// `tool_name` が root モードで一律拒否される file 書き込みツールかどうか。
#[must_use]
pub fn is_write_tool(tool_name: &str) -> bool {
    WRITE_TOOLS.contains(&tool_name)
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
    "ls-remote",
    "cat-file",
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
    "verify-commit",
    "verify-tag",
    "var",
    "help",
    "version",
];

/// サブコマンドより前の git グローバルオプションで、直後のトークンを値として消費するもの
/// （例: `git -C /path commit`）。その値をサブコマンドと取り違えないために使う。
const GIT_OPTS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
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
/// すべて拒否する。絶対パスの `git` は basename で認識し、read-only サブコマンド list の対象に残す。
#[must_use]
pub fn root_command_is_read_only(command: &str) -> bool {
    if command.trim().is_empty()
        || command
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | ';' | '&' | '|' | '>' | '<' | '`' | '$'))
    {
        return false;
    }
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
    if basename == "git" {
        let Some(subcommand) = git_subcommand_from_tokens(&tokens[1..]) else {
            return false;
        };
        return READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand)
            && !tokens.iter().any(|token| {
                token == "-o"
                    || token == "--output"
                    || token.starts_with("--output=")
                    || token == "--exec-path"
                    || token.starts_with("--exec-path=")
            });
    }
    matches!(
        basename,
        "pwd"
            | "ls"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "stat"
            | "test"
            | "true"
            | "false"
            | "which"
            | "rg"
            | "grep"
    )
}

fn git_subcommand_from_tokens(tokens: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token.starts_with('-') {
            index += 1;
            if GIT_OPTS_WITH_VALUE.contains(&token) {
                index += 1;
            }
        } else {
            return Some(token);
        }
    }
    None
}

/// `target` が `worktree` の外へ解決されるとき真。相対 `target` は `worktree`（エージェントの
/// cwd）を基準にするため常に内側に留まる。絶対パス、または `..` で外へ出る相対パスは、正規化した
/// 形が worktree 配下でないとき escape する。比較は component 単位なので、名前 prefix を共有する
/// 兄弟（`…/sessions/work` と `…/sessions/work2`）は「内側」と見なさない。
#[must_use]
pub fn escapes_worktree(worktree: &Path, target: &Path) -> bool {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        worktree.join(target)
    };
    !normalize(&absolute).starts_with(normalize(worktree))
}

/// ツールの対象パスが `root`（session worktree）の外へ解決されるかどうか。true のとき呼び出し側は
/// 拒否する。新規ファイルのため末尾の存在しない component は許容し、存在する symlink 付き ancestor
/// まで解決してから字句的に付け直す。`root` / `cwd` を canonicalize できない、または cwd が root の
/// 外にあるケースは、安全のため escape 扱い（true）にする（fail-closed）。
///
/// # Panics
///
/// 実際には panic しない。`cwd` を canonicalize した絶対パスを基準にするため `normalized` は常に
/// 絶対パスで、ファイルシステムのルートという存在する ancestor を必ず持ち、その ancestor は
/// 先に存在確認しているため canonicalize も成功する。
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
    let normalized = normalize(&absolute);
    // 存在する最深 ancestor まで遡り、そこを canonicalize（symlink 解決）してから、越えた
    // 未作成 component を字句的に付け直す。`normalized` は絶対パスなのでルートという存在する
    // ancestor を必ず持ち、その ancestor は存在するため canonicalize も成功する。
    let existing = normalized
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .expect("an absolute path always has the existing filesystem root as an ancestor");
    let mut resolved = std::fs::canonicalize(existing).expect("an existing ancestor canonicalizes");
    // `existing` は `normalized` の ancestor なので strip_prefix は必ず成功する。万一失敗しても
    // 空を付け足す（＝存在する ancestor のまま）保守的な挙動に倒す。
    resolved.push(normalized.strip_prefix(existing).unwrap_or(Path::new("")));
    !resolved.starts_with(root)
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

    const WT: &str = "/repo/.usagi/sessions/work";

    #[test]
    fn a_file_under_the_worktree_stays_inside() {
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work/src/main.rs")
        ));
    }

    #[test]
    fn the_worktree_itself_is_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new(WT)));
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_worktree_and_stays_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new("src/lib.rs")));
    }

    #[test]
    fn an_absolute_path_into_the_parent_repo_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/src/main.rs")
        ));
    }

    #[test]
    fn a_relative_path_climbing_out_with_dotdot_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("../../../src/main.rs")
        ));
    }

    #[test]
    fn dotdot_that_stays_inside_does_not_escape() {
        // `work/src/../Cargo.toml` は `work/Cargo.toml` に畳み込まれる。まだ内側。
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("src/../Cargo.toml")
        ));
    }

    #[test]
    fn a_sibling_worktree_sharing_a_name_prefix_escapes() {
        // component 単位の包含: 文字列 prefix が一致しても `work2` は `work` の配下ではない。
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work2/src/main.rs")
        ));
    }

    #[test]
    fn dotdot_at_the_root_does_not_climb_above_it() {
        // `/..` は `/` に正規化される。worktree 配下ではないので escape 扱い（panic も wrap もしない）。
        assert!(escapes_worktree(Path::new(WT), Path::new("/../etc/passwd")));
    }

    #[test]
    fn normalize_drops_a_leading_current_dir_component() {
        // 先頭の `.` は `Path::components` が唯一保持する `CurDir` 形（途中の `.` は既に畳まれる）。
        // 直接正規化すると skip され、実 component だけが残る。
        assert_eq!(normalize(Path::new("./a/b")), PathBuf::from("a/b"));
    }

    #[test]
    fn a_session_worktree_path_is_recognized() {
        assert!(is_session_worktree(Path::new("/repo/.usagi/sessions/work")));
        assert!(is_session_worktree(Path::new(
            "/repo/.usagi/sessions/work/src"
        )));
    }

    #[test]
    fn the_workspace_root_and_unrelated_paths_are_not_session_worktrees() {
        // コーディネータの cwd は repo root。`.usagi/sessions` セグメントを持たない。
        assert!(!is_session_worktree(Path::new("/repo")));
        // `sessions` 子を持たない `.usagi`（例: issue ストア）。
        assert!(!is_session_worktree(Path::new("/repo/.usagi/issues")));
        // `.usagi` 親を持たない `sessions` は該当しない。
        assert!(!is_session_worktree(Path::new("/repo/sessions/work")));
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
            "git status",
            "git log --oneline",
            "git diff HEAD~1",
            "git show abc123",
            "git rev-parse HEAD",
            "git ls-files",
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
        // `-C <path>` / `-c <cfg>` は値トークンを消費する。サブコマンドはその後ろ。
        assert!(command_mutates_repo("git -C /repo commit -m x"));
        assert!(command_mutates_repo("git -c user.name=x commit"));
        assert!(!command_mutates_repo("git -C /repo status"));
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
            "git log --output=/tmp/out",
            "echo $(git status)",
        ] {
            assert!(!root_command_is_read_only(command), "allowed {command}");
        }
        for command in [
            "git status",
            "/usr/bin/git log --oneline",
            "git -C /repo diff",
            "rg sandbox src",
            "/bin/ls -la",
        ] {
            assert!(root_command_is_read_only(command), "denied {command}");
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
}
