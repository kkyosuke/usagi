//! Resolving pull-request titles through the `gh` CLI.
//!
//! usagi harvests a session's PR **URLs** from its live terminal output (see
//! [`crate::presentation::tui::home::terminal::link::pr_links`]) but the terminal
//! rarely prints the PR's human title. To show `#<number>  <title>` in the PR
//! popup, the title is resolved out-of-band by asking `gh` — the GitHub CLI the
//! user already authenticates for their repositories.
//!
//! This module is the **pure** core of that feature: it builds the `gh` command
//! line ([`title_argv`]), parses the title out of its stdout ([`parse_title`]),
//! and fills the missing titles of a PR list through an injected runner
//! ([`resolve_titles`]). The real subprocess spawn lives in the
//! (coverage-excluded) terminal pool, which passes a runner that executes `gh`;
//! everything here is unit-tested against a fake runner so no network or `gh`
//! install is needed to cover it.

use crate::domain::workspace_state::PrLink;

/// The `gh` command line that prints PR `url`'s title as a single plain line.
/// `--jq .title` reduces the `--json title` object to the bare string so the
/// caller does not have to parse JSON.
pub fn title_argv(url: &str) -> Vec<String> {
    ["gh", "pr", "view", url, "--json", "title", "--jq", ".title"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The PR title parsed from `gh`'s stdout — the `--jq .title` output is the bare
/// title on its own line. Surrounding whitespace and the trailing newline are
/// trimmed; blank output (a failed lookup, or a PR with no title) yields `None`,
/// so the caller leaves the PR untitled and a later pass can retry.
pub fn parse_title(stdout: &str) -> Option<String> {
    let title = stdout.trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// Fill in the titles still missing from `prs`, fetching each through `run` (a
/// `gh` invocation that returns the command's stdout, or `None` when it could not
/// be run or exited non-zero). Already-titled PRs are skipped so a title is
/// fetched at most once. Returns whether any title was newly filled — the caller
/// persists the list only then, sparing a disk write when nothing changed.
pub fn resolve_titles(
    prs: &mut [PrLink],
    run: &mut dyn FnMut(&[String]) -> Option<String>,
) -> bool {
    let mut changed = false;
    for pr in prs.iter_mut() {
        if pr.title.is_some() {
            continue;
        }
        if let Some(title) = run(&title_argv(&pr.url)).as_deref().and_then(parse_title) {
            pr.title = Some(title);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_argv_asks_gh_for_the_bare_title() {
        assert_eq!(
            title_argv("https://github.com/o/r/pull/7"),
            vec![
                "gh",
                "pr",
                "view",
                "https://github.com/o/r/pull/7",
                "--json",
                "title",
                "--jq",
                ".title",
            ]
        );
    }

    #[test]
    fn parse_title_trims_and_rejects_blank_output() {
        assert_eq!(
            parse_title("Add PR titles\n").as_deref(),
            Some("Add PR titles")
        );
        assert_eq!(parse_title("  spaced  ").as_deref(), Some("spaced"));
        // A failed lookup / titleless PR prints nothing → no title.
        assert_eq!(parse_title(""), None);
        assert_eq!(parse_title("   \n"), None);
    }

    #[test]
    fn resolve_titles_fills_missing_skips_titled_and_reports_change() {
        let mut prs = vec![
            PrLink::new(1, "https://github.com/o/r/pull/1"),
            PrLink::new(2, "https://github.com/o/r/pull/2"),
        ];
        prs[1].title = Some("already known".to_string());

        let mut calls: Vec<Vec<String>> = Vec::new();
        let changed = resolve_titles(&mut prs, &mut |argv: &[String]| {
            calls.push(argv.to_vec());
            Some("fetched\n".to_string())
        });

        assert!(changed);
        // Only the untitled PR was queried; the titled one was left untouched.
        assert_eq!(calls, vec![title_argv("https://github.com/o/r/pull/1")]);
        assert_eq!(prs[0].title.as_deref(), Some("fetched"));
        assert_eq!(prs[1].title.as_deref(), Some("already known"));
    }

    #[test]
    fn resolve_titles_reports_no_change_when_the_runner_yields_nothing() {
        let mut prs = vec![PrLink::new(3, "https://github.com/o/r/pull/3")];
        // A failed `gh` (None) and a blank title both leave the PR untitled.
        assert!(!resolve_titles(&mut prs, &mut |_: &[String]| None));
        assert_eq!(prs[0].title, None);
        assert!(!resolve_titles(&mut prs, &mut |_: &[String]| Some(
            "  \n".to_string()
        )));
        assert_eq!(prs[0].title, None);
    }

    #[test]
    fn resolve_titles_over_an_empty_list_never_runs_the_fetch() {
        // The same runner is called on an empty list (which must not invoke it) and
        // then on a real one (which must), so its "not called" behaviour is pinned
        // without leaving its body unexercised.
        let mut run = |_: &[String]| Some("x\n".to_string());
        assert!(!resolve_titles(&mut [], &mut run));
        let mut prs = vec![PrLink::new(1, "https://github.com/o/r/pull/1")];
        assert!(resolve_titles(&mut prs, &mut run));
        assert_eq!(prs[0].title.as_deref(), Some("x"));
    }
}
