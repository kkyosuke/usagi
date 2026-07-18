//! The `PrLink` entity: a pull request discovered for a session, with the
//! bookkeeping the background `gh` enrichment needs.
//!
//! A session's terminal output is scanned for pull-request URLs; each becomes a
//! [`PrLink`] rendered as a `#<number>` badge. The link carries its lifecycle
//! [`PrState`] (open / merged / dismissed) plus retry/backoff bookkeeping for the
//! out-of-band `gh pr view` enrichment that fills in the title and auto-detected
//! state. A session usually shows several, rolled up by
//! [`PrLink::aggregate`] and de-duplicated by [`PrLink::pr_key`].

use serde::{Deserialize, Serialize};

// serde's `skip_serializing_if` hands each predicate `&field`, so the references
// below are required by that contract despite the types being trivially copyable.

/// `true` when a boolean is its `false` default, so an unpinned PR omits the
/// `pinned` key from persisted files.
#[allow(clippy::trivially_copy_pass_by_ref)]
#[coverage(off)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// `true` when a counter is its zero default, so a never-failed PR omits the
/// `attempts` key.
#[allow(clippy::trivially_copy_pass_by_ref)]
#[coverage(off)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// A pull request discovered for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    /// The pull request number — the `<N>` of the `/pull/<N>` path. Shown as
    /// `#<number>`.
    pub number: u32,
    /// The full URL to open in the browser when the badge is clicked.
    pub url: String,
    /// The pull request's title, resolved out-of-band via the `gh` CLI and shown
    /// next to the `#<number>`. `None` until fetched (or when the fetch failed);
    /// omitted from persisted files when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The PR's lifecycle state. Defaults to [`PrState::Open`]; an unrecognised
    /// stored value degrades to it, and an older file without the field loads as
    /// `Open`. Omitted from persisted files when `Open`.
    #[serde(default, skip_serializing_if = "PrState::is_open")]
    pub state: PrState,
    /// Whether [`state`](Self::state) was set by the user rather than derived. A
    /// pinned state is authoritative: `gh` auto-detection never overrides it.
    /// Defaults to `false`, omitted from persisted files when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// When the background `gh pr view` enrichment last checked this PR. Used to
    /// re-poll auto-managed open PRs even when the terminal prints no new output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<chrono::DateTime<chrono::Utc>>,
    /// The earliest time a failed/open lookup should be retried. Backoff state is
    /// persisted so restarting the TUI does not hammer `gh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry: Option<chrono::DateTime<chrono::Utc>>,
    /// Consecutive failed lookup attempts. Reset to zero on a successful lookup.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    /// Transient UI flag set while a worker is refreshing this PR. Never written
    /// to disk, so a TUI restart cannot leave a PR stuck "refreshing".
    #[serde(skip)]
    pub refreshing: bool,
    /// Last lookup error, if any. Shown only as a quiet hint and cleared on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup_error: Option<String>,
}

impl PrLink {
    /// A PR link with no title yet resolved — the shape freshly parsed from a
    /// pull-request URL. The title is filled in later by the enrichment worker.
    #[must_use]
    #[coverage(off)]
    pub fn new(number: u32, url: impl Into<String>) -> Self {
        Self {
            number,
            url: url.into(),
            title: None,
            state: PrState::Open,
            pinned: false,
            last_checked: None,
            next_retry: None,
            attempts: 0,
            refreshing: false,
            lookup_error: None,
        }
    }

    /// Whether this PR is dismissed (hidden). A dismissed PR is kept as a
    /// tombstone but excluded from the badge count and the popup's default view.
    #[must_use]
    #[coverage(off)]
    pub fn is_dismissed(&self) -> bool {
        self.state == PrState::Dismissed
    }

    /// Whether this PR shows in the badge and the popup's default view — every
    /// state except [`PrState::Dismissed`].
    #[must_use]
    #[coverage(off)]
    pub fn is_visible(&self) -> bool {
        !self.is_dismissed()
    }

    /// How many of `prs` are visible (not dismissed) — the number the sidebar's
    /// `#<count>` badge shows.
    #[must_use]
    #[coverage(off)]
    pub fn visible_count(prs: &[PrLink]) -> usize {
        prs.iter().filter(|p| p.is_visible()).count()
    }

    /// The canonical identity used to de-duplicate PR links: the URL truncated at
    /// the pull-request number, dropping any trailing path segment
    /// (`/pull/412/files`), query, or fragment. This makes `.../pull/412` and
    /// `.../pull/412/files` count as **one** PR. A URL with no recognisable
    /// `/pull/<N>` is its own key (returned whole).
    #[must_use]
    #[coverage(off)]
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
    /// still lacks one. Dismissal and a user pin are sticky across the fold.
    #[must_use]
    #[coverage(off)]
    pub fn aggregate(prs: impl IntoIterator<Item = PrLink>) -> Vec<PrLink> {
        let mut out: Vec<PrLink> = Vec::new();
        for pr in prs {
            if let Some(existing) = out.iter_mut().find(|p| p.pr_key() == pr.pr_key()) {
                // Read these before moving `pr.title` out below (a partial move
                // would otherwise forbid borrowing `pr` again).
                let dismissed = pr.is_dismissed();
                let pinned = pr.pinned;
                if existing.title.is_none() {
                    existing.title = pr.title;
                }
                // Dismissal is sticky: if any worktree hid this PR it stays hidden
                // folded, so a tombstone is never resurrected by a sibling that
                // still lists it. A user-pinned state on either side is preserved.
                if dismissed {
                    existing.state = PrState::Dismissed;
                }
                existing.pinned |= pinned;
            } else {
                out.push(pr);
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
    /// Merged — set automatically when `gh` reports the PR merged, or manually.
    Merged,
    /// Dismissed (hidden) — kept as a tombstone so a re-detected URL is not
    /// re-surfaced, but excluded from the badge count and the popup's default view.
    Dismissed,
    /// Open — the default for a freshly detected PR. Also the state an
    /// unrecognised stored token degrades to. `#[serde(other)]` makes it the
    /// catch-all, so it must stay the last variant.
    #[default]
    #[serde(other)]
    Open,
}

impl PrState {
    /// Whether this is the default [`Open`](Self::Open) state — the
    /// `skip_serializing_if` predicate that keeps `open` out of persisted files.
    /// Takes `&self` because serde's `skip_serializing_if` requires it.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    #[coverage(off)]
    fn is_open(&self) -> bool {
        matches!(self, PrState::Open)
    }
}

/// The byte offset just past the pull-request number in a `/pull/<N>` URL, or
/// `None` when the URL carries no `/pull/<digits>` segment. Used by
/// [`PrLink::pr_key`] to truncate a URL to its canonical PR identity.
#[coverage(off)]
fn pull_number_end(url: &str) -> Option<usize> {
    let marker = "/pull/";
    let after = url.find(marker)? + marker.len();
    let digits = url[after..].bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then_some(after + digits)
}

#[cfg(test)]
mod tests;
