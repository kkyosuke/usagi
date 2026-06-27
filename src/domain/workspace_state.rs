//! Per-repository workspace state.
//!
//! While [`crate::domain::workspace::Workspace`] is a *global* registry entry
//! (stored under `~/.usagi`), the types here describe the state of a single
//! repository and its worktrees. They are persisted inside the repository
//! itself, under `<repo>/.usagi/state.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a branch relative to its working tree, its remote, and
/// the default branch.
///
/// The states form a progression — `New` → (`Dirty`) → `Local` → `Pushed` →
/// `Synced` — but a branch does not march through them in order: it is
/// re-derived from git on every refresh, so editing files reads `Dirty`,
/// committing reads `Local`, pushing reads `Pushed`, and a branch the default
/// has moved past reads `Synced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    /// Freshly cut and untouched: a clean working tree with no commits of its
    /// own and the default branch has not moved past it (even with the default).
    /// This is the state a session branch starts in, before any work. Also the
    /// default an unreadable / unknown stored status degrades to.
    #[default]
    New,
    /// The working tree has uncommitted changes (modified, staged, or untracked
    /// files) — work in progress that has not been committed yet.
    Dirty,
    /// Clean tree with commits of its own that have not been pushed (no upstream
    /// tracking branch).
    Local,
    /// Clean tree with commits of its own and an upstream tracking branch (the
    /// branch has been pushed but is not yet merged).
    Pushed,
    /// The default branch has moved past this branch (it is behind with no
    /// commits of its own ahead): everything the branch carried is now on the
    /// integration branch, so it reads as `synced` — merged / up to date. Older
    /// `state.json` spelled this `"merged"` then `"up_to_date"`; both aliases
    /// keep that data loading (it is now written as `"synced"`).
    #[serde(alias = "merged", alias = "up_to_date")]
    Synced,
}

impl BranchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BranchStatus::New => "new",
            BranchStatus::Dirty => "dirty",
            BranchStatus::Local => "local",
            BranchStatus::Pushed => "pushed",
            BranchStatus::Synced => "synced",
        }
    }

    /// Derive a branch's lifecycle status from facts already gathered about it:
    ///
    /// - `dirty` — its working tree has uncommitted changes.
    /// - `counts` — its commits ahead of / behind the default branch, as
    ///   `Some((ahead, behind))`; `None` for a branch not measured against the
    ///   default (the default branch itself, a detached HEAD) or when the read
    ///   failed.
    /// - `has_upstream` — it has an upstream tracking branch.
    ///
    /// The order of checks:
    ///
    /// 1. **dirty** wins regardless of commit topology: there is work here that
    ///    has not been committed.
    /// 2. Otherwise, by commits *ahead of* the default branch:
    ///    - **ahead > 0** → `Pushed` if it has an upstream, else `Local`.
    ///    - **ahead == 0** → `Synced` if the default has moved past it
    ///      (behind > 0), else `New` (freshly cut, no work yet).
    ///
    /// A branch with no `counts` (default / detached / unread) skips the
    /// ahead/behind step and falls through to `Local` / `Pushed` by its upstream
    /// state. The pure derivation lives here; the usecase gathers the git facts.
    pub fn derive(dirty: bool, counts: Option<(usize, usize)>, has_upstream: bool) -> BranchStatus {
        if dirty {
            return BranchStatus::Dirty;
        }
        if let Some((ahead, behind)) = counts {
            if ahead == 0 {
                return if behind > 0 {
                    BranchStatus::Synced
                } else {
                    BranchStatus::New
                };
            }
        }
        if has_upstream {
            BranchStatus::Pushed
        } else {
            BranchStatus::Local
        }
    }

    /// Rank by lifecycle progress: `New` < `Dirty` < `Local` < `Pushed` <
    /// `Synced`. Used to aggregate a session's repositories into its
    /// least-progressed status.
    fn rank(self) -> u8 {
        match self {
            BranchStatus::New => 0,
            BranchStatus::Dirty => 1,
            BranchStatus::Local => 2,
            BranchStatus::Pushed => 3,
            BranchStatus::Synced => 4,
        }
    }

    /// Aggregate the per-repository statuses of one session's branch into a
    /// single status: the *least-progressed* of them. So a session reads as
    /// `synced` only when every repository's branch is up to date, and `pushed`
    /// only when none is still local/dirty/new — a conservative summary where
    /// `synced` always means "no un-merged work anywhere". An empty iterator
    /// yields `New`.
    pub fn aggregate(statuses: impl IntoIterator<Item = BranchStatus>) -> BranchStatus {
        statuses
            .into_iter()
            .min_by_key(|s| s.rank())
            .unwrap_or(BranchStatus::New)
    }
}

impl std::fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The added / removed line counts of a worktree's cumulative diff against its
/// repository's default branch — the size of the work a session has done so far,
/// shown as the sidebar's `+N -M` badge so a glance separates the sessions that
/// have progressed from the ones still untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffStat {
    /// Lines added (the `+N` half of the badge).
    pub added: usize,
    /// Lines removed (the `-M` half of the badge).
    pub removed: usize,
}

impl DiffStat {
    /// Whether the diff is empty — no lines added or removed, so a session even
    /// with its default branch shows no badge.
    pub fn is_empty(self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// Sum the per-repository diffs of one session into the single total its
    /// sidebar row shows. `None` entries (a repository even with its default, or
    /// one whose diff was not measured) contribute nothing; the result is `None`
    /// when every repository contributes nothing, so a session with no work shows
    /// no badge — mirroring how [`BranchStatus::aggregate`] rolls statuses up.
    pub fn aggregate(diffs: impl IntoIterator<Item = Option<DiffStat>>) -> Option<DiffStat> {
        let total = diffs
            .into_iter()
            .flatten()
            .fold(DiffStat::default(), |acc, d| DiffStat {
                added: acc.added + d.added,
                removed: acc.removed + d.removed,
            });
        (!total.is_empty()).then_some(total)
    }
}

/// How far a worktree's branch has diverged from its repository's default branch,
/// in **commits**: `ahead` are commits on the branch the default lacks, `behind`
/// are commits on the default the branch lacks. Shown on the sidebar as `↑N ↓M`
/// (the line-count [`DiffStat`] badge sits beside it), so a glance tells whether a
/// session is unmerged work (ahead) or stale relative to the default (behind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AheadBehind {
    /// Commits on the branch but not on the default (the `↑N` half).
    pub ahead: usize,
    /// Commits on the default but not on the branch (the `↓M` half).
    pub behind: usize,
}

impl AheadBehind {
    /// Whether the branch is even with its default — no commits ahead or behind,
    /// so the row shows no `↑↓` marker.
    pub fn is_empty(self) -> bool {
        self.ahead == 0 && self.behind == 0
    }

    /// Sum the per-repository ahead/behind counts of one session into the single
    /// total its sidebar row shows. `None` entries (a repository even with its
    /// default, or one not measured) contribute nothing; the result is `None` when
    /// every repository is even, mirroring [`DiffStat::aggregate`].
    pub fn aggregate(counts: impl IntoIterator<Item = Option<AheadBehind>>) -> Option<AheadBehind> {
        let total = counts
            .into_iter()
            .flatten()
            .fold(AheadBehind::default(), |acc, c| AheadBehind {
                ahead: acc.ahead + c.ahead,
                behind: acc.behind + c.behind,
            });
        (!total.is_empty()).then_some(total)
    }
}

/// A pull request discovered for a worktree: its number and the URL to open.
///
/// usagi does not query GitHub for this — it is harvested by scanning the
/// embedded agent's terminal output for a pull-request URL of the form
/// `https://<host>/<owner>/<repo>/pull/<N>` (see
/// [`crate::presentation::tui::home::terminal::link::pr_link`]). The sidebar
/// shows `#<number>` and a click opens [`url`](Self::url) in the default browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    /// The pull request number — the `<N>` of the `/pull/<N>` path. Shown as
    /// `#<number>`.
    pub number: u32,
    /// The full URL to open in the browser when the badge is clicked.
    pub url: String,
}

impl PrLink {
    /// Roll a session's per-worktree pull requests up into the single one its
    /// sidebar row shows: the first worktree that carries one (mirroring how
    /// [`crate::presentation::tui::home::state`] takes the first worktree's
    /// `head` / `upstream` as the session's representative detail). `None` when
    /// no worktree of the session has a PR.
    pub fn aggregate(prs: impl IntoIterator<Item = Option<PrLink>>) -> Option<PrLink> {
        prs.into_iter().flatten().next()
    }
}

/// State of a single worktree (a branch checked out into a directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeState {
    /// Branch checked out in this worktree. `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Short commit hash currently checked out.
    pub head: String,
    /// `true` for the repository's primary (main) worktree.
    #[serde(default)]
    pub primary: bool,
    /// Upstream tracking branch (e.g. `origin/feature`), if any.
    #[serde(default)]
    pub upstream: Option<String>,
    /// Lifecycle status of the checked-out branch. An unrecognised stored value
    /// (e.g. one written by a newer usagi) degrades to [`BranchStatus::New`]
    /// rather than failing the whole `state.json` load — see
    /// [`crate::domain::serde_fallback`]. It is re-derived from git on the next
    /// refresh regardless.
    #[serde(
        default,
        deserialize_with = "crate::domain::serde_fallback::or_default"
    )]
    pub status: BranchStatus,
    /// The worktree's cumulative diff against its repository's default branch —
    /// the sidebar's `+N -M` badge. `None` when not measured (the default branch
    /// itself, a detached HEAD, an unreadable diff) or when the tree is even with
    /// the default (an empty diff); omitted from the file when absent, and an
    /// older file without it loads as `None`. Re-derived from git on each
    /// refresh, like [`status`](Self::status).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffStat>,
    /// How far the branch has diverged from its default in **commits** — the
    /// sidebar's `↑N ↓M` marker. `None` when not measured (the default branch
    /// itself, a detached HEAD, an unreadable range) or when the branch is even
    /// with the default; omitted from the file when absent, and an older file
    /// without it loads as `None`. Re-derived from git on each refresh, like
    /// [`status`](Self::status) and [`diff`](Self::diff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead_behind: Option<AheadBehind>,
    /// The pull request discovered for this worktree, or `None` when none has been
    /// seen. Unlike the git-derived fields above this is **not** re-read from git on
    /// refresh: it is harvested from the embedded agent's terminal output (a
    /// `/pull/<N>` URL) and persisted so the sidebar keeps showing `#<number>`
    /// across restarts. Omitted from the file when absent, and an older file
    /// without it loads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrLink>,
    /// When this worktree's state was last refreshed.
    pub updated_at: DateTime<Utc>,
}

/// A session created under `.usagi/sessions/<name>/`: a parallel working tree
/// spanning every repository found under the workspace root (each as a git
/// worktree on the session branch) plus any copied non-git files.
///
/// Sessions are the single unit of state usagi tracks: each carries the git
/// status of its per-repository worktrees, so a workspace is fully described by
/// its sessions — even when the root itself is not a git repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session name (also the branch name created in every repository). This is
    /// the session's identity: commands (`session switch`, removal) target it,
    /// so it never changes once created.
    pub name: String,
    /// An optional sidebar label that overrides [`name`](Self::name) in the home
    /// screen's session list, without touching the branch / identity. `None`
    /// (the default, and omitted from the file) shows the `name` as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// A free-form, multi-line note attached to the session — scratch space for
    /// what it is for, what is left to do, links, and so on. Display / UX only:
    /// it never affects the session's identity or its branches. `None` (the
    /// default, and omitted from the file) means no note has been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
    pub root: PathBuf,
    /// One entry per repository that received a worktree, with its git status.
    pub worktrees: Vec<WorktreeState>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last *touched*: switched to, or observed producing
    /// terminal/agent activity. Drives the sidebar's freshness ("heat") dot.
    /// `None` (the default, and omitted from older files) means it has never been
    /// touched since creation, so callers fall back to
    /// [`created_at`](Self::created_at) via [`last_active_or_created`](Self::last_active_or_created).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<DateTime<Utc>>,
}

impl SessionRecord {
    /// The label shown in the sidebar: the custom [`display_name`](Self::display_name)
    /// when set, otherwise the session [`name`](Self::name).
    pub fn display_label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The session's note, or `None` when none has been written.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The reference time for the freshness ("heat") dot: the persisted
    /// [`last_active`](Self::last_active), or [`created_at`](Self::created_at) when
    /// the session has never been touched.
    pub fn last_active_or_created(&self) -> DateTime<Utc> {
        self.last_active.unwrap_or(self.created_at)
    }
}

/// State of a workspace: the sessions created under it.
///
/// There is no workspace-wide default branch — a workspace may span several git
/// repositories with differing defaults (`main`, `master`, …), so each
/// worktree's status is classified against *its own* repository's default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Sessions created under `.usagi/sessions/`, across all repositories in the
    /// workspace tree. Empty (and omitted from older files) when none exist.
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    /// When the state was last refreshed from git.
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceState {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn branch_status_as_str_and_display_match() {
        for (status, text) in [
            (BranchStatus::New, "new"),
            (BranchStatus::Dirty, "dirty"),
            (BranchStatus::Local, "local"),
            (BranchStatus::Pushed, "pushed"),
            (BranchStatus::Synced, "synced"),
        ] {
            assert_eq!(status.as_str(), text);
            assert_eq!(format!("{status}"), text);
        }
    }

    #[test]
    fn derive_classifies_from_dirty_counts_and_upstream() {
        use BranchStatus::*;
        // Dirty wins regardless of commit topology or upstream.
        assert_eq!(BranchStatus::derive(true, Some((3, 0)), true), Dirty);
        assert_eq!(BranchStatus::derive(true, None, false), Dirty);
        // ahead == 0: behind the default → Synced; level with it → New.
        assert_eq!(BranchStatus::derive(false, Some((0, 2)), true), Synced);
        assert_eq!(BranchStatus::derive(false, Some((0, 0)), true), New);
        // ahead > 0: Pushed with an upstream, else Local.
        assert_eq!(BranchStatus::derive(false, Some((1, 0)), true), Pushed);
        assert_eq!(BranchStatus::derive(false, Some((1, 0)), false), Local);
        // No counts (default branch / detached / unread): falls through to the
        // upstream state, skipping the ahead/behind step.
        assert_eq!(BranchStatus::derive(false, None, true), Pushed);
        assert_eq!(BranchStatus::derive(false, None, false), Local);
    }

    #[test]
    fn aggregate_reports_the_least_progressed_status() {
        use BranchStatus::*;
        // Uniform sets keep their status.
        assert_eq!(BranchStatus::aggregate([Synced, Synced]), Synced);
        assert_eq!(BranchStatus::aggregate([Pushed, Pushed]), Pushed);
        // Mixed sets fall to the least-progressed member, regardless of order.
        assert_eq!(BranchStatus::aggregate([Synced, Local]), Local);
        assert_eq!(BranchStatus::aggregate([Pushed, Synced]), Pushed);
        assert_eq!(BranchStatus::aggregate([Synced, Pushed, Local]), Local);
        // Dirty and New outrank a committed branch as "least progressed".
        assert_eq!(BranchStatus::aggregate([Pushed, Dirty]), Dirty);
        assert_eq!(BranchStatus::aggregate([Synced, New]), New);
        assert_eq!(BranchStatus::aggregate([Dirty, New]), New);
        // A single repository reports its own status; an empty set is `New`.
        assert_eq!(BranchStatus::aggregate([Synced]), Synced);
        assert_eq!(BranchStatus::aggregate([]), New);
    }

    #[test]
    fn branch_status_serializes_to_snake_case_and_reads_legacy_aliases() {
        let json = serde_json::to_string(&BranchStatus::Synced).unwrap();
        assert_eq!(json, "\"synced\"");
        let parsed: BranchStatus = serde_json::from_str("\"pushed\"").unwrap();
        assert_eq!(parsed, BranchStatus::Pushed);
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"new\"").unwrap(),
            BranchStatus::New
        );
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"dirty\"").unwrap(),
            BranchStatus::Dirty
        );
        // Older state.json spelled the synced status "merged", then "up_to_date";
        // both aliases keep that data loading.
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"merged\"").unwrap(),
            BranchStatus::Synced
        );
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"up_to_date\"").unwrap(),
            BranchStatus::Synced
        );
    }

    #[test]
    fn diff_stat_is_empty_only_when_both_counts_are_zero() {
        assert!(DiffStat::default().is_empty());
        assert!(DiffStat {
            added: 0,
            removed: 0
        }
        .is_empty());
        assert!(!DiffStat {
            added: 1,
            removed: 0
        }
        .is_empty());
        assert!(!DiffStat {
            added: 0,
            removed: 1
        }
        .is_empty());
    }

    #[test]
    fn diff_stat_aggregate_sums_repos_and_drops_an_all_empty_session() {
        // Per-repository diffs sum; `None` and empty entries contribute nothing.
        assert_eq!(
            DiffStat::aggregate([
                Some(DiffStat {
                    added: 12,
                    removed: 3
                }),
                None,
                Some(DiffStat {
                    added: 4,
                    removed: 1
                }),
            ]),
            Some(DiffStat {
                added: 16,
                removed: 4
            })
        );
        // A session whose repositories all contribute nothing shows no badge.
        assert_eq!(DiffStat::aggregate([None, Some(DiffStat::default())]), None);
        assert_eq!(DiffStat::aggregate(std::iter::empty()), None);
    }

    #[test]
    fn ahead_behind_is_empty_only_when_both_counts_are_zero() {
        assert!(AheadBehind::default().is_empty());
        assert!(!AheadBehind {
            ahead: 1,
            behind: 0
        }
        .is_empty());
        assert!(!AheadBehind {
            ahead: 0,
            behind: 1
        }
        .is_empty());
    }

    #[test]
    fn ahead_behind_aggregate_sums_repos_and_drops_an_all_even_session() {
        // Per-repository counts sum; `None` and even entries contribute nothing.
        assert_eq!(
            AheadBehind::aggregate([
                Some(AheadBehind {
                    ahead: 2,
                    behind: 1
                }),
                None,
                Some(AheadBehind {
                    ahead: 3,
                    behind: 0
                }),
            ]),
            Some(AheadBehind {
                ahead: 5,
                behind: 1
            })
        );
        // A session whose repositories are all even shows no marker.
        assert_eq!(
            AheadBehind::aggregate([None, Some(AheadBehind::default())]),
            None
        );
        assert_eq!(AheadBehind::aggregate(std::iter::empty()), None);
    }

    #[test]
    fn pr_link_aggregate_takes_the_first_worktree_with_a_pr() {
        let a = PrLink {
            number: 12,
            url: "https://github.com/o/r/pull/12".to_string(),
        };
        let b = PrLink {
            number: 34,
            url: "https://github.com/o/r/pull/34".to_string(),
        };
        // The first `Some` wins; `None` entries are skipped.
        assert_eq!(PrLink::aggregate([None, Some(a.clone()), Some(b)]), Some(a));
        // No worktree carries a PR → no badge.
        assert_eq!(PrLink::aggregate([None, None]), None);
        assert_eq!(PrLink::aggregate(std::iter::empty()), None);
    }

    #[test]
    fn pr_is_omitted_when_absent_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        // No PR → the key is dropped from the file and an older file parses.
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("\"pr\""));

        // A discovered PR is stored, and round-trips through JSON.
        state.sessions[0].worktrees[0].pr = Some(PrLink {
            number: 412,
            url: "https://github.com/KKyosuke/usagi/pull/412".to_string(),
        });
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"pr\":{\"number\":412,"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
    }

    #[test]
    fn diff_is_omitted_when_absent_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        // No diff → the key is dropped from the file and an older file parses.
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("diff"));

        // A measured diff is stored, and round-trips through JSON.
        state.sessions[0].worktrees[0].diff = Some(DiffStat {
            added: 12,
            removed: 3,
        });
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"diff\":{\"added\":12,\"removed\":3}"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
    }

    fn sample_worktree() -> WorktreeState {
        WorktreeState {
            branch: Some("feature-x".to_string()),
            path: PathBuf::from("/repo/.usagi/sessions/feature-x/app-a"),
            head: "abc1234".to_string(),
            primary: false,
            upstream: Some("origin/feature-x".to_string()),
            status: BranchStatus::Pushed,
            diff: None,
            ahead_behind: None,
            pr: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn new_state_starts_with_no_sessions() {
        assert!(WorkspaceState::new().sessions.is_empty());
        // `default()` delegates to `new()`, so it is also empty.
        assert!(WorkspaceState::default().sessions.is_empty());
    }

    #[test]
    fn workspace_state_round_trips_through_json() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn display_label_falls_back_to_name_then_prefers_display_name() {
        let mut session = SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        };
        // No override → the session name is the label.
        assert_eq!(session.display_label(), "feature-x");
        session.display_name = Some("My Feature".to_string());
        assert_eq!(session.display_label(), "My Feature");
    }

    #[test]
    fn display_name_is_omitted_from_json_when_absent_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: Some("Nice name".to_string()),
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"display_name\":\"Nice name\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // Cleared again → the key is dropped, and an older file without it parses.
        state.sessions[0].display_name = None;
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("display_name"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
    }

    #[test]
    fn note_is_omitted_when_absent_round_trips_when_set_and_reads_legacy_files() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        // No note → the accessor is `None` and the key is dropped from the file.
        assert_eq!(state.sessions[0].note(), None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("note"));

        // A multi-line note is stored, exposed, and round-trips through JSON.
        state.sessions[0].note = Some("line 1\nline 2".to_string());
        assert_eq!(state.sessions[0].note(), Some("line 1\nline 2"));
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"note\":\"line 1\\nline 2\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // An older file without a `note` key still parses (defaults to `None`).
        let legacy = r#"{"sessions":[{"name":"x","root":"/r","worktrees":[],"created_at":"2026-06-13T05:01:18.659149Z"}],"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.sessions[0].note(), None);
    }

    #[test]
    fn last_active_is_omitted_when_absent_falls_back_to_created_at_and_round_trips() {
        let created = Utc.with_ymd_and_hms(2026, 6, 13, 5, 0, 0).unwrap();
        let mut session = SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: created,
            last_active: None,
        };
        // Never touched → the heat reference falls back to `created_at` and the
        // key is dropped from the file.
        assert_eq!(session.last_active_or_created(), created);
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("last_active"));

        // Touched → the reference is `last_active`, and it round-trips.
        let touched = Utc.with_ymd_and_hms(2026, 6, 14, 9, 30, 0).unwrap();
        session.last_active = Some(touched);
        assert_eq!(session.last_active_or_created(), touched);
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("last_active"));
        assert_eq!(
            serde_json::from_str::<SessionRecord>(&json).unwrap(),
            session
        );

        // An older file without `last_active` parses to `None`.
        let legacy =
            r#"{"name":"x","root":"/r","worktrees":[],"created_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: SessionRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.last_active, None);
    }

    #[test]
    fn sessions_default_to_empty_when_absent() {
        // An older file without a `sessions` key still parses (defaults empty).
        let legacy = r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions.is_empty());
    }
}
