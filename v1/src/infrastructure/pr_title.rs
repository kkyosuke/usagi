//! Resolving pull-request titles and merge state through the `gh` CLI.
//!
//! usagi harvests a session's PR **URLs** from its live terminal output (see
//! [`crate::presentation::tui::home::terminal::link::pr_links`]) but the terminal
//! rarely prints the PR's human title, and never whether it has since merged. To
//! show `#<number>  <title>` in the PR popup and mark a merged PR, both are
//! resolved out-of-band by asking `gh` — the GitHub CLI the user already
//! authenticates for their repositories.
//!
//! This module is the **pure** core of that feature: it builds the `gh` command
//! line ([`view_argv`]), parses the title and state out of its stdout
//! ([`parse_view`]), and fills a PR list's missing titles and auto-detected merge
//! state through an injected runner ([`resolve`]). The real subprocess spawn lives
//! in the (coverage-excluded) terminal pool, which passes a runner that executes
//! `gh`; everything here is unit-tested against a fake runner so no network or
//! `gh` install is needed to cover it.

use crate::domain::workspace_state::{PrLink, PrState};
use chrono::{DateTime, Duration, Utc};

/// How soon an auto-managed open PR is re-polled after a successful lookup. This
/// lets OPEN -> MERGED land in the TUI even when the agent never prints the PR URL
/// again.
pub const OPEN_REFRESH_AFTER: Duration = Duration::seconds(30);
/// First retry delay after a failed lookup.
pub const RETRY_BASE: Duration = Duration::seconds(5);
/// Maximum retry delay for repeated failures.
pub const RETRY_MAX: Duration = Duration::minutes(5);

/// The `gh` command line that prints PR `url`'s title and state as a JSON object,
/// e.g. `{"title":"Fix the thing","state":"MERGED"}`. `state` is one of `OPEN`,
/// `CLOSED`, or `MERGED`.
pub fn view_argv(url: &str) -> Vec<String> {
    ["gh", "pr", "view", url, "--json", "title,state"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// A PR's title and merge state, parsed from [`view_argv`]'s JSON stdout.
#[derive(Debug, PartialEq, Eq)]
pub struct PrView {
    /// The PR title, or `None` when `gh` returned none / a blank one.
    pub title: Option<String>,
    /// Whether `gh` reports the PR merged (`state == "MERGED"`).
    pub merged: bool,
}

/// A completed lookup with enough information to update the PR's retry metadata.
pub enum LookupOutcome {
    Found(PrView),
    Failed(String),
}

/// Parse `gh`'s JSON stdout into a [`PrView`]. Invalid or empty output (a failed
/// lookup, or `gh` not installed) yields `None`, so the caller leaves the PR as it
/// is and a later pass can retry. A present-but-blank title is normalised to
/// `None`; surrounding whitespace is trimmed.
pub fn parse_view(stdout: &str) -> Option<PrView> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let title = value
        .get("title")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let merged = value.get("state").and_then(|s| s.as_str()) == Some("MERGED");
    Some(PrView { title, merged })
}

/// Fill in the titles still missing from `prs` and auto-detect merges, fetching
/// each through `run` (a `gh` invocation returning the command's stdout, or `None`
/// when it could not be run or exited non-zero). Returns whether anything changed —
/// the caller persists the list only then, sparing a disk write when nothing did.
///
/// A PR is queried only when a fact could still change: it needs a title, or its
/// state is still the auto-managed [`PrState::Open`]. A **dismissed** PR is left
/// untouched (it is a tombstone), and a **pinned** state — one the user set from
/// the popup — is authoritative and never overridden by `gh`. Once a PR is titled
/// and merged (or dismissed/pinned) it is skipped, so `gh` is not re-polled
/// forever.
pub fn resolve(prs: &mut [PrLink], run: &mut dyn FnMut(&[String]) -> Option<String>) -> bool {
    let mut changed = false;
    for pr in prs.iter_mut() {
        if pr.is_dismissed() {
            continue;
        }
        let need_title = pr.title.is_none();
        let need_state = !pr.pinned && pr.state == PrState::Open;
        if !need_title && !need_state {
            continue;
        }
        let Some(view) = run(&view_argv(&pr.url)).as_deref().and_then(parse_view) else {
            continue;
        };
        if need_title {
            if let Some(title) = view.title {
                pr.title = Some(title);
                changed = true;
            }
        }
        if need_state && view.merged {
            pr.state = PrState::Merged;
            changed = true;
        }
    }
    changed
}

/// Whether this PR should be sent to the background lookup worker at `now`.
///
/// Only auto-managed open PRs are re-polled. Dismissed, pinned, and already
/// merged PRs are left alone; a transient in-flight lookup also suppresses a
/// duplicate enqueue from another pane or watcher tick.
pub fn lookup_due(pr: &PrLink, now: DateTime<Utc>) -> bool {
    if pr.refreshing || pr.is_dismissed() || pr.pinned || pr.state == PrState::Merged {
        return false;
    }
    if pr.title.is_none() {
        return pr.next_retry.is_none_or(|at| at <= now);
    }
    pr.state == PrState::Open && pr.next_retry.is_none_or(|at| at <= now)
}

/// Mark that a worker has started refreshing this PR. Returns whether the flag
/// changed, so callers can avoid redundant store writes/UI updates.
pub fn mark_refreshing(pr: &mut PrLink) -> bool {
    if pr.refreshing {
        return false;
    }
    pr.refreshing = true;
    true
}

/// Apply a lookup result to `pr`, updating title/state and retry metadata.
pub fn apply_lookup(pr: &mut PrLink, outcome: LookupOutcome, now: DateTime<Utc>) -> bool {
    let before = pr.clone();
    pr.refreshing = false;
    pr.last_checked = Some(now);
    match outcome {
        LookupOutcome::Found(view) => {
            pr.attempts = 0;
            pr.lookup_error = None;
            if pr.title.is_none() {
                pr.title = view.title;
            }
            if !pr.pinned && pr.state == PrState::Open && view.merged {
                pr.state = PrState::Merged;
            }
            pr.next_retry = if !pr.pinned && pr.state == PrState::Open {
                Some(now + OPEN_REFRESH_AFTER)
            } else {
                None
            };
        }
        LookupOutcome::Failed(error) => {
            pr.attempts = pr.attempts.saturating_add(1);
            pr.lookup_error = Some(error);
            pr.next_retry = Some(now + retry_delay(pr.attempts));
        }
    }
    *pr != before
}

/// Exponential backoff for failed lookups, capped so an old error still retries
/// occasionally while the TUI is open.
pub fn retry_delay(attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(10);
    let factor = 1_i32.checked_shl(exponent).unwrap_or(1 << 10);
    (RETRY_BASE * factor).min(RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_argv_asks_gh_for_the_title_and_state_json() {
        assert_eq!(
            view_argv("https://github.com/o/r/pull/7"),
            vec![
                "gh",
                "pr",
                "view",
                "https://github.com/o/r/pull/7",
                "--json",
                "title,state",
            ]
        );
    }

    #[test]
    fn parse_view_reads_title_and_merge_state() {
        assert_eq!(
            parse_view(r#"{"title":"Add PR titles","state":"MERGED"}"#),
            Some(PrView {
                title: Some("Add PR titles".to_string()),
                merged: true,
            })
        );
        // An open PR: a title, not merged.
        assert_eq!(
            parse_view(r#"{"title":"  spaced  ","state":"OPEN"}"#),
            Some(PrView {
                title: Some("spaced".to_string()),
                merged: false,
            })
        );
        // A closed-but-unmerged PR is not merged.
        assert_eq!(
            parse_view(r#"{"title":"x","state":"CLOSED"}"#),
            Some(PrView {
                title: Some("x".to_string()),
                merged: false,
            })
        );
    }

    #[test]
    fn parse_view_normalises_blank_titles_and_rejects_bad_json() {
        // A present-but-blank title becomes None; a missing state is not merged.
        assert_eq!(
            parse_view(r#"{"title":"   "}"#),
            Some(PrView {
                title: None,
                merged: false,
            })
        );
        // A missing title key is None too.
        assert_eq!(
            parse_view(r#"{"state":"MERGED"}"#),
            Some(PrView {
                title: None,
                merged: true,
            })
        );
        // A failed lookup / gh-not-installed prints nothing parseable → None.
        assert_eq!(parse_view(""), None);
        assert_eq!(parse_view("not json at all"), None);
    }

    #[test]
    fn resolve_fills_titles_skips_titled_and_reports_change() {
        let mut prs = vec![
            PrLink::new(1, "https://github.com/o/r/pull/1"),
            PrLink::new(2, "https://github.com/o/r/pull/2"),
        ];
        prs[1].title = Some("already known".to_string());
        // #2 is already titled and open, so it is still polled for a merge below.

        let mut calls: Vec<Vec<String>> = Vec::new();
        let changed = resolve(&mut prs, &mut |argv: &[String]| {
            calls.push(argv.to_vec());
            Some(r#"{"title":"fetched","state":"OPEN"}"#.to_string())
        });

        assert!(changed);
        // Both open PRs were queried — the untitled one for its title, the titled
        // one to check whether it has merged.
        assert_eq!(
            calls,
            vec![
                view_argv("https://github.com/o/r/pull/1"),
                view_argv("https://github.com/o/r/pull/2"),
            ]
        );
        assert_eq!(prs[0].title.as_deref(), Some("fetched"));
        // The already-known title is not clobbered.
        assert_eq!(prs[1].title.as_deref(), Some("already known"));
        assert_eq!(prs[0].state, PrState::Open);
    }

    #[test]
    fn resolve_marks_a_merged_pr() {
        let mut prs = vec![PrLink::new(3, "https://github.com/o/r/pull/3")];
        // One runner reused across both passes, counting its calls — so its body is
        // exercised on the first (queried) pass and merely *not* re-run on the
        // second, without a never-run closure leaving dead lines.
        let mut calls = 0;
        {
            let mut run = |_: &[String]| {
                calls += 1;
                Some(r#"{"title":"done","state":"MERGED"}"#.to_string())
            };
            // First pass queries and merges; the second has nothing left to learn
            // (titled and merged) so it does not query again.
            assert!(resolve(&mut prs, &mut run));
            assert!(!resolve(&mut prs, &mut run));
        }
        assert_eq!(calls, 1);
        assert_eq!(prs[0].title.as_deref(), Some("done"));
        assert_eq!(prs[0].state, PrState::Merged);
    }

    #[test]
    fn resolve_leaves_dismissed_and_pinned_prs_alone() {
        let mut dismissed = PrLink::new(4, "https://github.com/o/r/pull/4");
        dismissed.state = PrState::Dismissed;
        let mut pinned_open = PrLink::new(5, "https://github.com/o/r/pull/5");
        pinned_open.pinned = true; // user kept it open
        pinned_open.title = Some("kept open".to_string()); // and it is already titled
                                                           // A plain open PR alongside them, so the runner is genuinely exercised and
                                                           // we can assert *which* PRs were queried.
        let plain = PrLink::new(7, "https://github.com/o/r/pull/7");
        let mut prs = vec![dismissed, pinned_open, plain];

        let mut calls: Vec<Vec<String>> = Vec::new();
        // The dismissed PR is skipped entirely and the pinned-open PR is
        // authoritative (so `gh` reporting it merged does not flip it); only the
        // plain open PR is queried.
        let changed = resolve(&mut prs, &mut |argv: &[String]| {
            calls.push(argv.to_vec());
            Some(r#"{"title":"t","state":"MERGED"}"#.to_string())
        });
        assert!(changed);
        assert_eq!(calls, vec![view_argv("https://github.com/o/r/pull/7")]);
        assert_eq!(prs[0].state, PrState::Dismissed);
        assert!(prs[0].title.is_none());
        assert_eq!(prs[1].state, PrState::Open);
        assert_eq!(prs[2].state, PrState::Merged);
    }

    #[test]
    fn resolve_reports_no_change_when_the_runner_yields_nothing() {
        let mut prs = vec![PrLink::new(6, "https://github.com/o/r/pull/6")];
        // A failed `gh` (None) and unparseable output both leave the PR untouched.
        assert!(!resolve(&mut prs, &mut |_: &[String]| None));
        assert_eq!(prs[0].title, None);
        assert!(!resolve(&mut prs, &mut |_: &[String]| Some(
            "garbage".to_string()
        )));
        assert_eq!(prs[0].title, None);
        assert_eq!(prs[0].state, PrState::Open);
    }

    #[test]
    fn resolve_over_an_empty_list_never_runs_the_fetch() {
        // The same runner is called on an empty list (which must not invoke it) and
        // then on a real one (which must), so its "not called" behaviour is pinned
        // without leaving its body unexercised.
        let mut run = |_: &[String]| Some(r#"{"title":"x","state":"OPEN"}"#.to_string());
        assert!(!resolve(&mut [], &mut run));
        let mut prs = vec![PrLink::new(1, "https://github.com/o/r/pull/1")];
        assert!(resolve(&mut prs, &mut run));
        assert_eq!(prs[0].title.as_deref(), Some("x"));
    }

    #[test]
    fn lookup_due_repolls_open_prs_and_skips_final_or_manual_states() {
        let now = Utc::now();
        let mut open = PrLink::new(1, "https://github.com/o/r/pull/1");
        open.title = Some("known".to_string());
        open.next_retry = Some(now);
        assert!(lookup_due(&open, now));
        open.next_retry = Some(now + Duration::seconds(1));
        assert!(!lookup_due(&open, now));

        let mut untitled = PrLink::new(2, "https://github.com/o/r/pull/2");
        assert!(lookup_due(&untitled, now));
        untitled.next_retry = Some(now);
        assert!(lookup_due(&untitled, now));
        untitled.next_retry = Some(now + Duration::seconds(1));
        assert!(!lookup_due(&untitled, now));
        untitled.refreshing = true;
        assert!(!lookup_due(&untitled, now));

        let mut merged = PrLink::new(3, "https://github.com/o/r/pull/3");
        merged.state = PrState::Merged;
        assert!(!lookup_due(&merged, now));
        let mut pinned = PrLink::new(4, "https://github.com/o/r/pull/4");
        pinned.pinned = true;
        assert!(!lookup_due(&pinned, now));
        let mut dismissed = PrLink::new(5, "https://github.com/o/r/pull/5");
        dismissed.state = PrState::Dismissed;
        assert!(!lookup_due(&dismissed, now));
    }

    #[test]
    fn mark_refreshing_reports_whether_it_changed_the_flag() {
        let mut pr = PrLink::new(6, "https://github.com/o/r/pull/6");
        assert!(mark_refreshing(&mut pr));
        assert!(pr.refreshing);
        assert!(!mark_refreshing(&mut pr));
    }

    #[test]
    fn apply_lookup_schedules_open_refresh_and_clears_failure_state() {
        let now = Utc::now();
        let mut pr = PrLink::new(7, "https://github.com/o/r/pull/7");
        pr.refreshing = true;
        pr.attempts = 2;
        pr.lookup_error = Some("old".to_string());

        assert!(apply_lookup(
            &mut pr,
            LookupOutcome::Found(PrView {
                title: Some("Title".to_string()),
                merged: false,
            }),
            now,
        ));
        assert_eq!(pr.title.as_deref(), Some("Title"));
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.last_checked, Some(now));
        assert_eq!(pr.next_retry, Some(now + OPEN_REFRESH_AFTER));
        assert_eq!(pr.attempts, 0);
        assert!(!pr.refreshing);
        assert!(pr.lookup_error.is_none());
    }

    #[test]
    fn apply_lookup_marks_merged_without_rescheduling() {
        let now = Utc::now();
        let mut pr = PrLink::new(8, "https://github.com/o/r/pull/8");
        assert!(apply_lookup(
            &mut pr,
            LookupOutcome::Found(PrView {
                title: None,
                merged: true,
            }),
            now,
        ));
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.next_retry, None);
    }

    #[test]
    fn apply_lookup_failure_uses_exponential_backoff() {
        let now = Utc::now();
        let mut pr = PrLink::new(9, "https://github.com/o/r/pull/9");
        assert!(apply_lookup(
            &mut pr,
            LookupOutcome::Failed("gh timed out".to_string()),
            now,
        ));
        assert_eq!(pr.attempts, 1);
        assert_eq!(pr.next_retry, Some(now + RETRY_BASE));
        assert_eq!(pr.lookup_error.as_deref(), Some("gh timed out"));

        assert!(apply_lookup(
            &mut pr,
            LookupOutcome::Failed("still failing".to_string()),
            now,
        ));
        assert_eq!(pr.attempts, 2);
        assert_eq!(pr.next_retry, Some(now + RETRY_BASE * 2));
    }
}
