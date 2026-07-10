//! Per-repository workspace state.
//!
//! While [`crate::domain::workspace::Workspace`] is a *global* registry entry
//! (stored under `~/.usagi`), the types here describe the state of a single
//! repository and its worktrees. They are persisted inside the repository
//! itself, under `<repo>/.usagi/state.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::settings::AgentCli;

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
/// usagi does not query GitHub for this — it is harvested by scanning live
/// embedded terminal output for pull-request URLs of the form
/// `https://<host>/<owner>/<repo>/pull/<N>` (see
/// [`crate::presentation::tui::home::terminal::link::harvest_pr_links`]). The sidebar
/// shows `#<number>` and a click opens [`url`](Self::url) in the default browser.
/// A session may carry several — one per repository it touches, or several opened
/// over its life — so they are kept as a list ([`WorktreeState::pr`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    /// The pull request number — the `<N>` of the `/pull/<N>` path. Shown as
    /// `#<number>`.
    pub number: u32,
    /// The full URL to open in the browser when the badge is clicked.
    pub url: String,
    /// The pull request's title, resolved out-of-band via the `gh` CLI (see
    /// [`crate::infrastructure::pr_title`]) and shown next to the `#<number>` in
    /// the PR popup. `None` until it has been fetched (or when the fetch failed);
    /// omitted from persisted files when absent, and an older file without it
    /// loads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The PR's lifecycle state — open, merged, or dismissed (hidden). Drives how
    /// the popup renders and lists it. Defaults to [`PrState::Open`]; an
    /// unrecognised stored value degrades to it via
    /// [`crate::domain::serde_fallback`], and an older file without the field
    /// loads as `Open`. Omitted from persisted files when `Open`.
    #[serde(
        default,
        skip_serializing_if = "PrState::is_open",
        deserialize_with = "crate::domain::serde_fallback::or_default"
    )]
    pub state: PrState,
    /// Whether [`state`](Self::state) was set by the user (from the popup) rather
    /// than derived. A pinned state is authoritative: the `gh` auto-detection
    /// ([`crate::infrastructure::pr_title`]) never overrides it. Defaults to
    /// `false`, and is omitted from persisted files when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

impl PrLink {
    /// A PR link with no title yet resolved — the shape freshly parsed from a
    /// pull-request URL. The title is filled in later by
    /// [`crate::infrastructure::pr_title`].
    pub fn new(number: u32, url: impl Into<String>) -> Self {
        Self {
            number,
            url: url.into(),
            title: None,
            state: PrState::Open,
            pinned: false,
        }
    }

    /// Whether this PR is dismissed (hidden). A dismissed PR is kept as a
    /// tombstone but excluded from the badge count and the popup's default view.
    pub fn is_dismissed(&self) -> bool {
        self.state == PrState::Dismissed
    }

    /// Whether this PR shows in the badge and the popup's default view — every
    /// state except [`PrState::Dismissed`].
    pub fn is_visible(&self) -> bool {
        !self.is_dismissed()
    }

    /// How many of `prs` are visible (not dismissed) — the number the sidebar's
    /// `#<count>` badge shows.
    pub fn visible_count(prs: &[PrLink]) -> usize {
        prs.iter().filter(|p| p.is_visible()).count()
    }

    /// The canonical identity used to de-duplicate PR links: the URL truncated at
    /// the pull-request number, dropping any trailing path segment
    /// (`/pull/412/files`), query, or fragment. This makes `.../pull/412` and
    /// `.../pull/412/files` count as **one** PR — the fix for the same PR showing
    /// several times when a command printed both the plain and the `/files` URL.
    /// A URL with no recognisable `/pull/<N>` is its own key (returned whole).
    pub fn pr_key(&self) -> &str {
        match pull_number_end(&self.url) {
            Some(end) => &self.url[..end],
            None => &self.url,
        }
    }

    /// Roll a session's per-worktree pull requests up into the single list its
    /// sidebar row shows: every worktree's PRs, in order, de-duplicated by
    /// [`pr_key`](Self::pr_key) so a PR shared across repositories — or seen with
    /// both its plain and `/files` URL — is listed once. When duplicates carry
    /// different titles the first sighting keeps its URL but adopts a title if it
    /// still lacks one, so an early untitled sighting is upgraded by a later
    /// titled one. Empty when no worktree of the session has a PR.
    pub fn aggregate(prs: impl IntoIterator<Item = PrLink>) -> Vec<PrLink> {
        let mut out: Vec<PrLink> = Vec::new();
        for pr in prs {
            match out.iter_mut().find(|p| p.pr_key() == pr.pr_key()) {
                Some(existing) => {
                    // Read these before moving `pr.title` out below (a partial move
                    // would otherwise forbid borrowing `pr` again).
                    let dismissed = pr.is_dismissed();
                    let pinned = pr.pinned;
                    if existing.title.is_none() {
                        existing.title = pr.title;
                    }
                    // Dismissal is sticky: if any worktree hid this PR it stays
                    // hidden folded, so a tombstone is never resurrected by a
                    // sibling worktree that still lists it. A user-pinned state on
                    // either side is likewise preserved.
                    if dismissed {
                        existing.state = PrState::Dismissed;
                    }
                    existing.pinned |= pinned;
                }
                None => out.push(pr),
            }
        }
        out
    }
}

/// The lifecycle state of a discovered pull request, controlling how the PR popup
/// renders and lists it (see [`PrLink::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// Open — shown with the `○` glyph. The default for a freshly detected PR.
    #[default]
    Open,
    /// Merged — shown with the `●` glyph. Set automatically when `gh` reports the
    /// PR merged, or manually from the popup.
    Merged,
    /// Dismissed (hidden) — kept as a tombstone so a re-detected URL is not
    /// re-surfaced, but excluded from the badge count and the popup's default
    /// view. Revealed by the popup's "hidden" toggle, from where it can be
    /// restored to [`Open`](Self::Open).
    Dismissed,
}

impl PrState {
    /// Whether this is the default [`Open`](Self::Open) state — the
    /// `skip_serializing_if` predicate that keeps `open` out of persisted files.
    fn is_open(&self) -> bool {
        matches!(self, PrState::Open)
    }
}

/// The byte offset just past the pull-request number in a `/pull/<N>` URL, or
/// `None` when the URL carries no `/pull/<digits>` segment. Used by
/// [`PrLink::pr_key`] to truncate a URL to its canonical PR identity.
fn pull_number_end(url: &str) -> Option<usize> {
    let marker = "/pull/";
    let after = url.find(marker)? + marker.len();
    let digits = url[after..].bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then_some(after + digits)
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
    /// The pull requests discovered for this worktree — one per `/pull/<N>` URL
    /// printed in a live embedded pane, in the order first seen (a session may
    /// open several across the repositories it touches). Unlike the git-derived
    /// fields above this is **not** re-read from git on refresh: it is harvested
    /// from terminal output and persisted so the sidebar keeps showing the
    /// `#<number>` badges across restarts. Empty (and omitted from the file) when
    /// none has been seen, and an older file without it loads empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pr: Vec<PrLink>,
    /// When this worktree's state was last refreshed.
    pub updated_at: DateTime<Utc>,
}

/// Per-session overrides for the agent CLI its pane launches, chosen when the
/// session is created or delegated (MCP `session_create` /
/// `session_delegate_issue`) and applied **ahead of the workspace's effective
/// settings** when the session's agent pane spawns — the interactive launch, the
/// startup pane recovery, and the background auto-start of a queued prompt all
/// honour it. This is what lets a coordinator send a light task to a small model
/// and a heavy design to a large one, session by session.
///
/// Both fields unset (the default, and omitted from `state.json`) means the
/// session follows the workspace `agent_cli` and the CLI's own default model,
/// exactly as before this override existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAgent {
    /// The agent CLI this session launches, overriding the workspace effective
    /// [`agent_cli`](crate::domain::settings::Settings::agent_cli). `None` defers
    /// to that setting. An unrecognised stored value degrades to `None` (defer)
    /// rather than failing the whole `state.json` — see
    /// [`crate::domain::serde_fallback`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::domain::serde_fallback::or_default"
    )]
    pub cli: Option<AgentCli>,
    /// The model the session's agent CLI runs, rendered by the adapter as that
    /// CLI's own model flag (`--model` for Claude, `-m` for Codex / Gemini). `None`
    /// lets the CLI use its configured default. The value is stored verbatim and
    /// shell-escaped at launch, so no allowlist is imposed — model names differ per
    /// CLI and change often, and pass-through keeps usagi from rejecting a model a
    /// newer CLI added.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl SessionAgent {
    /// Whether neither override is set, i.e. the session follows the workspace
    /// effective settings. Used to omit the field from `state.json` when empty.
    pub fn is_unset(&self) -> bool {
        self.cli.is_none() && self.model.is_none()
    }
}

/// Who launched a session — a person operating the TUI, or an agent driving the
/// MCP server — recorded once when the session is created so a later reader (a
/// coordinating agent polling `session_status`, an operator scanning the home
/// screen) can tell an automated session apart from a hand-made one.
///
/// The two real origins are [`Human`](Self::Human) and [`Mcp`](Self::Mcp); every
/// session usagi creates from here on carries one of them. [`Unknown`](Self::Unknown)
/// is only the degraded reading of a session recorded by an *older* usagi that
/// predates this field (its key is simply absent) — it is the default so such a
/// record still loads, and it is omitted from the file rather than fabricating a
/// `human` / `mcp` origin the old usagi never knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Origin not recorded: a session read from a `state.json` written before
    /// usagi tracked this. Also the value an unrecognised stored origin degrades
    /// to (see [`crate::domain::serde_fallback`]). Never written for a session
    /// usagi creates itself — those are always [`Human`](Self::Human) or
    /// [`Mcp`](Self::Mcp).
    #[default]
    Unknown,
    /// Created interactively by a person from the TUI home screen (選択 create).
    Human,
    /// Created by an agent through the MCP server — the `session_create` and
    /// `session_delegate_issue` tools.
    Mcp,
}

impl SessionOrigin {
    /// The lowercase token used in `state.json` and the MCP tool output
    /// (`"unknown"` / `"human"` / `"mcp"`), matching the `snake_case` serde
    /// rename so the string form has one source of truth.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionOrigin::Unknown => "unknown",
            SessionOrigin::Human => "human",
            SessionOrigin::Mcp => "mcp",
        }
    }

    /// Whether the origin was not recorded (the pre-field default). Used to omit
    /// the field from `state.json` so an untracked session stays lean, exactly as
    /// unset [`display_name`](SessionRecord::display_name) / [`agent`](SessionAgent)
    /// are omitted.
    pub fn is_unknown(&self) -> bool {
        matches!(self, SessionOrigin::Unknown)
    }
}

impl std::fmt::Display for SessionOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A session created under `.usagi/sessions/<name>/`: a parallel working tree
/// spanning every repository found under the workspace root (each as a git
/// worktree on the session branch) plus any copied non-git files.
///
/// Sessions are the single unit of state usagi tracks: each carries the git
/// status of its per-repository worktrees, so a workspace is fully described by
/// its sessions — even when the root itself is not a git repository.
/// `true` when a boolean is its `false` default, so a not-yet-done todo omits
/// the `done` key from `state.json`.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One entry in a session's (or the root's) lightweight checklist — the
/// `todos` section of the note scratchpad.
///
/// Unlike an issue in the store (`.usagi/issues/`, git-tracked, with status /
/// priority / dependencies), a todo is a throwaway, machine-local reminder for
/// the current session's work. Display / UX only, alongside
/// [`SessionRecord::note`](SessionRecord::note).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionTodo {
    /// The todo text. Trimmed and required to be non-empty by the usecase that
    /// writes it.
    pub text: String,
    /// Whether the todo is checked off. `false` (the default) is omitted from
    /// `state.json`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
}

impl SessionTodo {
    /// A fresh, unchecked todo with the given text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            done: false,
        }
    }
}

/// One append-only entry in a session's (or the root's) decision log — the
/// `decisions` section of the note scratchpad.
///
/// An agent records *why* it chose an approach, timestamped, so a coordinator
/// can follow a session's reasoning without replaying its whole transcript.
/// Display / UX only, machine-local like [`SessionRecord::note`](SessionRecord::note).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDecision {
    /// When the decision was recorded (RFC3339, UTC). Supplied by the caller so
    /// the usecase stays clock-free and testable.
    pub at: DateTime<Utc>,
    /// What was decided and why. Trimmed and required non-empty by the usecase.
    pub text: String,
}

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
    /// A lightweight checklist for this session's work — the `todos` section of
    /// the note scratchpad. Display / UX only, machine-local; distinct from the
    /// git-tracked issue store. Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<SessionTodo>,
    /// An append-only log of decisions an agent made while working this session —
    /// the `decisions` section of the note scratchpad. Display / UX only. Empty
    /// (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<SessionDecision>,
    /// The [`id`](crate::domain::settings::SessionLabelDef::id) of the manual
    /// status label the user has assigned to this session in 選択 (Overview), or
    /// `None` when unset. Resolved back to a
    /// [`SessionLabelDef`](crate::domain::settings::SessionLabelDef) through the
    /// effective [`SessionLabelMaster`](crate::domain::settings::SessionLabelMaster)
    /// for display; an id no longer in the master reads as unset. Purely a
    /// user-assigned tag — unlike [`WorktreeState::status`] it is never derived
    /// from git, so a workspace refresh leaves it untouched. `None` (the default)
    /// is omitted from the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_id: Option<String>,
    /// Per-session agent CLI / model overrides, applied ahead of the workspace's
    /// effective settings when this session's agent pane launches. Unset by
    /// default (the session follows the workspace `agent_cli` and the CLI's own
    /// default model); omitted from `state.json` when empty. See [`SessionAgent`].
    #[serde(default, skip_serializing_if = "SessionAgent::is_unset")]
    pub agent: SessionAgent,
    /// Who launched the session — a person via the TUI ([`SessionOrigin::Human`])
    /// or an agent via the MCP server ([`SessionOrigin::Mcp`]) — recorded once at
    /// creation and never changed afterwards (a workspace refresh leaves it
    /// untouched, like [`label_id`](Self::label_id)). Defaults to
    /// [`SessionOrigin::Unknown`] for a record written before usagi tracked this,
    /// and that default is omitted from `state.json`. An unrecognised stored value
    /// degrades to `Unknown` rather than failing the whole load — see
    /// [`crate::domain::serde_fallback`].
    #[serde(
        default,
        skip_serializing_if = "SessionOrigin::is_unknown",
        deserialize_with = "crate::domain::serde_fallback::or_default"
    )]
    pub origin: SessionOrigin,
    /// The name of the session this one was **started from** — the parent session
    /// the agent was running inside when it created this one through the MCP server
    /// (`session_create` / `session_delegate_issue`). This is the session-level
    /// lineage: it answers "which session did this session get started from?", so a
    /// tree of coordinator-and-children sessions can be reconstructed.
    ///
    /// `None` when there is no parent to record: a session a person cut in the TUI
    /// ([`SessionOrigin::Human`]), or one an agent created while running at the
    /// workspace root rather than inside a session (the root coordinator has no
    /// parent session). Recorded once at creation and never changed afterwards (a
    /// workspace refresh leaves it untouched, like [`origin`](Self::origin)).
    /// Omitted from `state.json` when absent, and an older file without the key
    /// loads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<String>,
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

    /// The session's todo checklist (empty when none have been added).
    pub fn todos(&self) -> &[SessionTodo] {
        &self.todos
    }

    /// The session's decision log (empty when none have been recorded).
    pub fn decisions(&self) -> &[SessionDecision] {
        &self.decisions
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
    /// A free-form, multi-line note attached to the workspace **root** (the `⌂
    /// root` row, which belongs to no session) — the same scratch space sessions
    /// carry in [`SessionRecord::note`], but for the workspace itself. Display /
    /// UX only. `None` (the default, and omitted from the file) means no root note
    /// has been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_note: Option<String>,
    /// The workspace **root**'s todo checklist — the `⌂ root` row's counterpart
    /// to [`SessionRecord::todos`]. Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_todos: Vec<SessionTodo>,
    /// The workspace **root**'s decision log — the `⌂ root` row's counterpart to
    /// [`SessionRecord::decisions`]. Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_decisions: Vec<SessionDecision>,
    /// When the state was last refreshed from git.
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceState {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            root_note: None,
            root_todos: Vec::new(),
            root_decisions: Vec::new(),
            updated_at: Utc::now(),
        }
    }

    /// The workspace root's note, or `None` when none has been written.
    pub fn root_note(&self) -> Option<&str> {
        self.root_note.as_deref()
    }

    /// The workspace root's todo checklist (empty when none have been added).
    pub fn root_todos(&self) -> &[SessionTodo] {
        &self.root_todos
    }

    /// The workspace root's decision log (empty when none have been recorded).
    pub fn root_decisions(&self) -> &[SessionDecision] {
        &self.root_decisions
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
    fn root_note_round_trips_and_is_omitted_when_absent() {
        // A default state has no root note; the accessor and the serialized form
        // both reflect that — the field is omitted from the file entirely.
        let mut state = WorkspaceState::new();
        assert_eq!(state.root_note(), None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("root_note"));
        // An older file with no `root_note` key still loads (the field defaults).
        let restored: WorkspaceState =
            serde_json::from_str(r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#).unwrap();
        assert_eq!(restored.root_note(), None);

        // A set root note round-trips through the file and the accessor.
        state.root_note = Some("root memo".to_string());
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("root_note"));
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.root_note(), Some("root memo"));
    }

    #[test]
    fn session_agent_is_unset_only_when_both_fields_are_none() {
        assert!(SessionAgent::default().is_unset());
        assert!(!SessionAgent {
            cli: Some(AgentCli::Claude),
            model: None,
        }
        .is_unset());
        assert!(!SessionAgent {
            cli: None,
            model: Some("opus".to_string()),
        }
        .is_unset());
    }

    #[test]
    fn session_agent_is_omitted_from_the_record_when_unset_and_round_trips_when_set() {
        // A default (unset) agent override is skipped entirely, so an older
        // `state.json` without the key still loads and a fresh one stays lean.
        let mut session = SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "s".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: SessionAgent::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from("/tmp/s"),
            worktrees: vec![],
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            last_active: None,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("agent"), "{json}");

        // A pinned CLI + model round-trips through the file.
        session.agent = SessionAgent {
            cli: Some(AgentCli::Gemini),
            model: Some("gemini-2.5-pro".to_string()),
        };
        let json = serde_json::to_string(&session).unwrap();
        let restored: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.agent.cli, Some(AgentCli::Gemini));
        assert_eq!(restored.agent.model.as_deref(), Some("gemini-2.5-pro"));
    }

    #[test]
    fn session_agent_degrades_an_unknown_cli_to_none_without_failing_the_load() {
        // A `cli` a newer usagi wrote (or a hand-edited typo) degrades to None
        // rather than failing the whole record — the model beside it still loads.
        let restored: SessionRecord = serde_json::from_str(
            r#"{
                "name": "s",
                "agent": {"cli": "future-cli", "model": "m"},
                "root": "/tmp/s",
                "worktrees": [],
                "created_at": "2026-06-13T05:01:18.659149Z"
            }"#,
        )
        .unwrap();
        assert_eq!(restored.agent.cli, None);
        assert_eq!(restored.agent.model.as_deref(), Some("m"));
    }

    #[test]
    fn session_origin_as_str_and_display_match() {
        for (origin, text) in [
            (SessionOrigin::Unknown, "unknown"),
            (SessionOrigin::Human, "human"),
            (SessionOrigin::Mcp, "mcp"),
        ] {
            assert_eq!(origin.as_str(), text);
            assert_eq!(format!("{origin}"), text);
        }
        // Only the pre-field default reports as unknown.
        assert!(SessionOrigin::default().is_unknown());
        assert!(SessionOrigin::Unknown.is_unknown());
        assert!(!SessionOrigin::Human.is_unknown());
        assert!(!SessionOrigin::Mcp.is_unknown());
    }

    #[test]
    fn session_origin_is_omitted_when_unknown_and_round_trips_when_set() {
        // The default (Unknown) origin is skipped entirely, so a session recorded
        // before usagi tracked provenance stays lean and its key is simply absent.
        let mut session = SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "s".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: SessionAgent::default(),
            origin: SessionOrigin::Unknown,
            started_from: None,
            root: PathBuf::from("/tmp/s"),
            worktrees: vec![],
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            last_active: None,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("origin"), "{json}");

        // A human origin serializes as "human" and round-trips.
        session.origin = SessionOrigin::Human;
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"origin\":\"human\""), "{json}");
        let restored: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.origin, SessionOrigin::Human);

        // As does an MCP origin.
        session.origin = SessionOrigin::Mcp;
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"origin\":\"mcp\""), "{json}");
        let restored: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.origin, SessionOrigin::Mcp);
    }

    #[test]
    fn session_origin_defaults_to_unknown_when_the_key_is_absent() {
        // An older `state.json` written before this field simply has no `origin`
        // key; it must still load, with the origin reading as Unknown rather than
        // fabricating a human / mcp provenance the old usagi never recorded.
        let restored: SessionRecord = serde_json::from_str(
            r#"{
                "name": "s",
                "root": "/tmp/s",
                "worktrees": [],
                "created_at": "2026-06-13T05:01:18.659149Z"
            }"#,
        )
        .unwrap();
        assert_eq!(restored.origin, SessionOrigin::Unknown);
    }

    #[test]
    fn session_origin_degrades_an_unknown_value_without_failing_the_load() {
        // An `origin` a newer usagi wrote (a future provenance) or a hand-edited
        // typo degrades to Unknown rather than failing the whole record — the
        // fields beside it still load.
        let restored: SessionRecord = serde_json::from_str(
            r#"{
                "name": "s",
                "origin": "cron",
                "root": "/tmp/s",
                "worktrees": [],
                "created_at": "2026-06-13T05:01:18.659149Z"
            }"#,
        )
        .unwrap();
        assert_eq!(restored.origin, SessionOrigin::Unknown);
        assert_eq!(restored.name, "s");
    }

    #[test]
    fn session_started_from_is_omitted_when_absent_and_round_trips_when_set() {
        // No parent recorded: the field is skipped entirely, so a root-launched or
        // interactively-created session stays lean and an older file without the
        // key still loads.
        let mut session = SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "child".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: SessionAgent::default(),
            origin: SessionOrigin::Mcp,
            started_from: None,
            root: PathBuf::from("/tmp/child"),
            worktrees: vec![],
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            last_active: None,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("started_from"), "{json}");

        // A recorded parent session round-trips through the file — this is the
        // lineage answer to "which session was this started from?".
        session.started_from = Some("coordinator".to_string());
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"started_from\":\"coordinator\""), "{json}");
        let restored: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.started_from.as_deref(), Some("coordinator"));
    }

    #[test]
    fn session_started_from_defaults_to_none_when_the_key_is_absent() {
        // An older `state.json` written before this field simply has no
        // `started_from` key; it must still load, with the parent reading as None.
        let restored: SessionRecord = serde_json::from_str(
            r#"{
                "name": "s",
                "root": "/tmp/s",
                "worktrees": [],
                "created_at": "2026-06-13T05:01:18.659149Z"
            }"#,
        )
        .unwrap();
        assert_eq!(restored.started_from, None);
    }

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
    fn pr_link_aggregate_collects_every_pr_and_drops_duplicate_urls() {
        let a = PrLink::new(12, "https://github.com/o/r/pull/12");
        let b = PrLink::new(34, "https://github.com/o/s/pull/34");
        // Every PR is collected, in order; a duplicate `url` is listed once.
        assert_eq!(
            PrLink::aggregate([a.clone(), b.clone(), a.clone()]),
            vec![a.clone(), b]
        );
        // No worktree carries a PR → empty.
        assert_eq!(PrLink::aggregate(std::iter::empty()), Vec::new());
    }

    #[test]
    fn pr_link_aggregate_folds_the_files_url_onto_the_plain_pr() {
        // The same PR seen with both its plain and its `/pull/<N>/files` URL is
        // one PR: the first sighting wins and the duplicate is dropped. This is
        // the fix for a PR showing several times in the popup.
        let plain = PrLink::new(7, "https://github.com/o/r/pull/7");
        let files = PrLink::new(7, "https://github.com/o/r/pull/7/files");
        assert_eq!(
            PrLink::aggregate([plain.clone(), files]),
            vec![plain.clone()]
        );
        // pr_key truncates at the number regardless of a trailing path or query.
        assert_eq!(plain.pr_key(), "https://github.com/o/r/pull/7");
        assert_eq!(
            PrLink::new(7, "https://github.com/o/r/pull/7?w=1").pr_key(),
            "https://github.com/o/r/pull/7"
        );
        // A URL with no `/pull/<N>` is its own key.
        assert_eq!(
            PrLink::new(0, "https://example.com/x").pr_key(),
            "https://example.com/x"
        );
    }

    #[test]
    fn pr_link_aggregate_upgrades_an_untitled_sighting_with_a_later_title() {
        let untitled = PrLink::new(9, "https://github.com/o/r/pull/9");
        let mut titled = PrLink::new(9, "https://github.com/o/r/pull/9/files");
        titled.title = Some("Fix the thing".to_string());
        let folded = PrLink::aggregate([untitled, titled]);
        assert_eq!(folded.len(), 1);
        // The first sighting's URL is kept, but it adopts the later title.
        assert_eq!(folded[0].url, "https://github.com/o/r/pull/9");
        assert_eq!(folded[0].title.as_deref(), Some("Fix the thing"));
    }

    #[test]
    fn pr_state_helpers_classify_visibility_and_count() {
        let open = PrLink::new(1, "https://github.com/o/r/pull/1");
        let mut merged = PrLink::new(2, "https://github.com/o/r/pull/2");
        merged.state = PrState::Merged;
        let mut hidden = PrLink::new(3, "https://github.com/o/r/pull/3");
        hidden.state = PrState::Dismissed;

        // Open and merged are visible; only dismissed is hidden.
        assert!(open.is_visible() && !open.is_dismissed());
        assert!(merged.is_visible() && !merged.is_dismissed());
        assert!(hidden.is_dismissed() && !hidden.is_visible());
        // The badge count is the visible ones — the dismissed PR is not counted.
        assert_eq!(PrLink::visible_count(&[open, merged, hidden]), 2);
        assert_eq!(PrLink::visible_count(&[]), 0);
    }

    #[test]
    fn pr_link_aggregate_keeps_a_dismissal_and_a_pin_sticky() {
        // The same PR seen open in one worktree and dismissed in another folds to a
        // single dismissed entry — a tombstone a sibling sighting cannot resurrect.
        let open = PrLink::new(5, "https://github.com/o/r/pull/5");
        let mut hidden = PrLink::new(5, "https://github.com/o/r/pull/5/files");
        hidden.state = PrState::Dismissed;
        hidden.pinned = true;
        let folded = PrLink::aggregate([open, hidden]);
        assert_eq!(folded.len(), 1);
        assert!(folded[0].is_dismissed());
        // The user-pinned flag survives the fold too.
        assert!(folded[0].pinned);

        // Order-independent: an open sighting after the dismissed one still folds
        // to dismissed (the existing dismissal wins, and the pin is OR-ed in).
        let mut hidden_first = PrLink::new(6, "https://github.com/o/r/pull/6");
        hidden_first.state = PrState::Dismissed;
        let open_second = PrLink::new(6, "https://github.com/o/r/pull/6");
        let folded = PrLink::aggregate([hidden_first, open_second]);
        assert_eq!(folded.len(), 1);
        assert!(folded[0].is_dismissed());
        assert!(!folded[0].pinned);
    }

    #[test]
    fn pr_is_omitted_when_empty_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        // No PR → the key is dropped from the file and an older file parses.
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("\"pr\""));

        // Discovered PRs are stored as a list, and round-trip through JSON.
        state.sessions[0].worktrees[0].pr = vec![
            PrLink::new(412, "https://github.com/KKyosuke/usagi/pull/412"),
            PrLink::new(98, "https://github.com/KKyosuke/other/pull/98"),
        ];
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"pr\":[{\"number\":412,"));
        // An untitled PR omits the `title` key; a resolved title round-trips.
        assert!(!json.contains("\"title\""));
        state.sessions[0].worktrees[0].pr[0].title = Some("Add PR titles".to_string());
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"title\":\"Add PR titles\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // The default `open` state and unpinned flag are omitted from the file (so
        // an older usagi parses it), but a non-default state and a pin round-trip.
        assert!(!json.contains("\"state\"") && !json.contains("\"pinned\""));
        state.sessions[0].worktrees[0].pr[1].state = PrState::Merged;
        state.sessions[0].worktrees[0].pr[1].pinned = true;
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"state\":\"merged\"") && json.contains("\"pinned\":true"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
        // An unknown future state degrades to the default rather than failing the
        // whole load (serde_fallback), while the rest of the PR still parses.
        let forged = json.replace("\"state\":\"merged\"", "\"state\":\"superseded\"");
        let loaded = serde_json::from_str::<WorkspaceState>(&forged).unwrap();
        assert_eq!(loaded.sessions[0].worktrees[0].pr[1].state, PrState::Open);
        assert!(loaded.sessions[0].worktrees[0].pr[1].pinned);
    }

    #[test]
    fn diff_is_omitted_when_absent_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
            pr: Vec::new(),
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
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: Some("Nice name".to_string()),
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
    fn todos_and_decisions_are_omitted_when_empty_round_trip_and_read_legacy_files() {
        let at = Utc.with_ymd_and_hms(2026, 6, 13, 5, 1, 18).unwrap();
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: at,
            last_active: None,
        });

        // Empty → accessors are empty slices and the keys are dropped everywhere.
        assert!(state.sessions[0].todos().is_empty());
        assert!(state.sessions[0].decisions().is_empty());
        assert!(state.root_todos().is_empty());
        assert!(state.root_decisions().is_empty());
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("todos"));
        assert!(!json.contains("decisions"));
        assert!(!json.contains("root_todos"));
        assert!(!json.contains("root_decisions"));

        // Session and root carry checklists and decision logs; the unchecked
        // `done` default is dropped from the file, a checked one is written.
        state.sessions[0].todos = vec![SessionTodo::new("write tests"), {
            let mut done = SessionTodo::new("draft design");
            done.done = true;
            done
        }];
        state.sessions[0].decisions = vec![SessionDecision {
            at,
            text: "store todos on SessionRecord for symmetry with note".to_string(),
        }];
        state.root_todos = vec![SessionTodo::new("cut a release")];
        state.root_decisions = vec![SessionDecision {
            at,
            text: "keep the scratchpad machine-local".to_string(),
        }];

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"text\":\"write tests\""));
        // The unchecked todo omits `done`; only the checked one serialises it.
        assert_eq!(json.matches("\"done\":true").count(), 1);
        assert!(!json.contains("\"done\":false"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
        assert_eq!(state.sessions[0].todos().len(), 2);
        assert_eq!(state.sessions[0].decisions().len(), 1);
        assert_eq!(state.root_todos().len(), 1);
        assert_eq!(state.root_decisions().len(), 1);

        // A legacy file without any of the new keys parses with empty collections.
        let legacy = r#"{"sessions":[{"name":"x","root":"/r","worktrees":[],"created_at":"2026-06-13T05:01:18.659149Z"}],"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions[0].todos().is_empty());
        assert!(parsed.sessions[0].decisions().is_empty());
        assert!(parsed.root_todos().is_empty());
        assert!(parsed.root_decisions().is_empty());
    }

    #[test]
    fn last_active_is_omitted_when_absent_falls_back_to_created_at_and_round_trips() {
        let created = Utc.with_ymd_and_hms(2026, 6, 13, 5, 0, 0).unwrap();
        let mut session = SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
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
    fn label_id_is_omitted_when_absent_round_trips_when_set_and_reads_legacy_files() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
            last_active: None,
        });
        // No label → the key is dropped from the file (an unset tag).
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("label_id"));

        // An assigned label id round-trips through JSON.
        state.sessions[0].label_id = Some("review".to_string());
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"label_id\":\"review\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // An older file without a `label_id` key parses (defaults to `None`).
        let legacy =
            r#"{"name":"x","root":"/r","worktrees":[],"created_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: SessionRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.label_id, None);
    }

    #[test]
    fn sessions_default_to_empty_when_absent() {
        // An older file without a `sessions` key still parses (defaults empty).
        let legacy = r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions.is_empty());
    }
}
