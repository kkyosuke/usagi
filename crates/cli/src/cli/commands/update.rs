//! `usagi update` — 最新版があるか確認する。
//!
//! リリースは GitHub の `v<major>.<minor>.<patch>` タグ。注入された core の [`GitRunner`]
//! seam で `git ls-remote --tags <repo>` を実行し、その出力から最大の semver を取り出して
//! 配布 version と比較し、結果を 1 行報告する。git 実行そのものは実 IO なので合成ルートが
//! 束ね、出力のパースとバージョン比較は以下の純粋関数に閉じてユニットテストする。

use std::io::{self, Write};
use std::path::Path;

use usagi_core::infrastructure::git::{GitOutput, GitRunner};

use crate::cli::{Run, RunOutcome};

/// usagi のリポジトリ URL（`git ls-remote` の対象）。ワークスペースの `repository` を継ぐ。
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// `usagi update` のハンドラ。`current` は配布 version、`git` はタグ取得に使う seam。
pub struct Update {
    pub current: String,
    pub git: Box<dyn GitRunner>,
}

impl Run for Update {
    #[coverage(off)]
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        // ls-remote は特定リポジトリに紐づかないので `-C` 先はカレントで十分。
        let output = self
            .git
            .run(Path::new("."), &["ls-remote", "--tags", REPOSITORY])
            .map_err(io::Error::other)?;
        writeln!(out, "{}", status_message(&self.current, &output))?;
        Ok(RunOutcome::Exit(0))
    }
}

/// `v1.2.3` / `1.2.3` を `(major, minor, patch)` に解釈する。3 数値でなければ `None`。
#[coverage(off)]
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let digits = text.strip_prefix('v').unwrap_or(text);
    let mut parts = digits.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    // `1.2.3.4` のように 4 つ目以降があるタグは弾く。
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// `git ls-remote --tags` の出力から最大の version を取り出す。1 つも無ければ `None`。
///
/// 各行は `<sha>\trefs/tags/<tag>` 形式で、peeled タグは末尾に `^{}` が付く。
#[coverage(off)]
fn latest_version(stdout: &str) -> Option<(u64, u64, u64)> {
    stdout
        .lines()
        .filter_map(|line| {
            let refname = line.split('\t').nth(1)?;
            let tag = refname.strip_prefix("refs/tags/")?;
            parse_version(tag.trim_end_matches("^{}"))
        })
        .max()
}

/// 現在の version と ls-remote 出力から報告する 1 行を組み立てる。
#[coverage(off)]
fn status_message(current: &str, output: &GitOutput) -> String {
    let Some(current_version) = parse_version(current) else {
        return "usagi: could not parse the current version".to_owned();
    };
    if !output.success {
        return "usagi: could not query the latest release".to_owned();
    }
    match latest_version(&output.stdout) {
        Some(latest) if latest > current_version => {
            let (major, minor, patch) = latest;
            format!("usagi {current}: update available -> {major}.{minor}.{patch}")
        }
        Some(_) => format!("usagi {current}: up to date"),
        None => "usagi: could not determine the latest release".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{latest_version, parse_version, status_message};
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome};
    use usagi_core::infrastructure::git::GitOutput;

    /// `git ls-remote --tags` 風の出力を組み立てる。
    #[coverage(off)]
    fn ls_remote(tags: &[&str]) -> GitOutput {
        let stdout = tags
            .iter()
            .map(|tag| format!("0000000000000000000000000000000000000000\trefs/tags/{tag}"))
            .collect::<Vec<_>>()
            .join("\n");
        GitOutput {
            success: true,
            stdout,
            stderr: String::new(),
        }
    }

    #[test]
    #[coverage(off)]
    fn parse_version_accepts_v_prefix_and_rejects_malformed() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    #[coverage(off)]
    fn latest_version_picks_the_maximum_and_ignores_junk() {
        let stdout = ls_remote(&["v1.2.0", "v1.10.0", "v1.3.0", "nightly", "v1.10.0^{}"]).stdout;
        assert_eq!(latest_version(&stdout), Some((1, 10, 0)));
        assert_eq!(latest_version("garbage-without-tabs"), None);
        assert_eq!(latest_version(""), None);
    }

    #[test]
    #[coverage(off)]
    fn status_message_reports_update_available() {
        let msg = status_message("2.6.0", &ls_remote(&["v2.6.0", "v2.7.0"]));
        assert_eq!(msg, "usagi 2.6.0: update available -> 2.7.0");
    }

    #[test]
    #[coverage(off)]
    fn status_message_reports_up_to_date() {
        let msg = status_message("2.7.0", &ls_remote(&["v2.6.0", "v2.7.0"]));
        assert_eq!(msg, "usagi 2.7.0: up to date");
    }

    #[test]
    #[coverage(off)]
    fn status_message_reports_unknown_when_no_valid_tags() {
        assert_eq!(
            status_message("2.6.0", &ls_remote(&["nightly"])),
            "usagi: could not determine the latest release"
        );
    }

    #[test]
    #[coverage(off)]
    fn status_message_reports_git_failure() {
        let failed = GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "fatal: unable to access".to_owned(),
        };
        assert_eq!(
            status_message("2.6.0", &failed),
            "usagi: could not query the latest release"
        );
    }

    #[test]
    #[coverage(off)]
    fn status_message_reports_bad_current_version() {
        assert_eq!(
            status_message("not-semver", &ls_remote(&["v2.7.0"])),
            "usagi: could not parse the current version"
        );
    }

    #[test]
    #[coverage(off)]
    fn handler_runs_via_dispatch() {
        // execute の StubGit は v9.9.10 を含み、現在 9.9.9 なので更新ありを報告する。
        let (outcome, output) = execute(Command::Update);
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(output.contains("update available"), "got: {output}");
    }
}
