//! Pull Request modal state and its snapshot reducer.
//!
//! The modal lists the daemon's canonical [`PrEntry`] rows for one [`Target`].
//! Two surfaces share that inventory: this modal and the sidebar badge, both
//! reading [`AppState::prs_for`]. The modal itself exists only while
//! [`Overlay::Prs`] is the foreground; an inventory request that has not
//! produced a modal yet is a request, not an overlay.

use usagi_core::domain::id::SessionId;
use usagi_core::domain::pr_inventory::{PrEntry, PrIdentity, PrState};
use usagi_core::domain::settings::PrAutoOpen;

use super::{AppKey, AppState, Effect, HomeMode, Notice, Overlay, Route, SafeError, Target};

impl AppState {
    /// Latest daemon PR rows for one target (workspace root or session).
    #[must_use]
    pub fn prs_for(&self, target: Target) -> Option<&[PrEntry]> {
        self.prs.get(&target).map(|(_, prs)| prs.as_slice())
    }
    /// Latest daemon PR rows for one stable session identity.
    #[must_use]
    pub fn session_prs(&self, session: SessionId) -> Option<&[PrEntry]> {
        self.prs_for(Target::Session(session))
    }
    /// Rows of one target that the modal may show, in inventory order.
    /// Dismissed rows are never visible, whatever the active status tab is.
    fn visible_prs(&self, target: Target) -> Vec<PrEntry> {
        self.prs_for(target)
            .map(|prs| filtered_prs(prs, PrFilter::All))
            .unwrap_or_default()
    }
    /// Whether a PR modal may take the foreground without displacing something
    /// the user is already looking at.
    fn can_surface_pr_modal(&self) -> bool {
        self.overlay.is_none() && !self.workspace_drawer_open()
    }
    /// Show `overlay` as the foreground PR modal. Opening one always resolves
    /// the pending request, so the two never describe the same target at once.
    fn open_pr_modal(&mut self, overlay: PrOverlay) {
        self.overlay = Some(Overlay::Prs);
        self.pr_overlay = Some(overlay);
        self.pr_request = None;
        self.preview_overlay = None;
    }
    /// Drop the PR modal and any request behind it, releasing the foreground
    /// only when the modal actually owns it.
    fn close_pr_modal(&mut self) {
        self.pr_overlay = None;
        self.pr_request = None;
        if self.overlay == Some(Overlay::Prs) {
            self.overlay = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrFilter {
    #[default]
    All,
    Open,
    Closed,
    Merged,
}

impl PrFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Open,
            Self::Open => Self::Closed,
            Self::Closed => Self::Merged,
            Self::Merged => Self::All,
        }
    }

    pub(super) fn previous(self) -> Self {
        match self {
            Self::All => Self::Merged,
            Self::Open => Self::All,
            Self::Closed => Self::Open,
            Self::Merged => Self::Closed,
        }
    }

    /// Status tabs in their horizontal navigation order.
    pub const TABS: [Self; 4] = [Self::All, Self::Open, Self::Closed, Self::Merged];

    #[must_use]
    pub const fn tab_index(self) -> usize {
        match self {
            Self::All => 0,
            Self::Open => 1,
            Self::Closed => 2,
            Self::Merged => 3,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }

    /// Whether this status tab shows a PR in `state`. Visibility is a separate
    /// question: a dismissed PR is in no tab at all.
    const fn admits(self, state: PrState) -> bool {
        match self {
            Self::All => true,
            Self::Open => matches!(state, PrState::Open),
            Self::Closed => matches!(state, PrState::Closed),
            Self::Merged => matches!(state, PrState::Merged),
        }
    }
}
/// active target の Pull Request 一覧 overlay state。
///
/// 一覧 [`PrEntry`] は domain データで、素材は port（[`Effect::LoadPullRequests`]）から
/// [`BackendEvent::PullRequestsLoaded`] として還流する。reducer が所有するのは選択位置と
/// 表示可能なエラーだけで、URL の妥当性検証や browser 起動は executor 側に残す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrOverlay {
    target: Target,
    prs: Vec<PrEntry>,
    selected: usize,
    error: Option<SafeError>,
    filter: PrFilter,
}
impl PrOverlay {
    /// Modal listing `prs` on the unfiltered status tab, with the cursor on
    /// `detected` when that row is present and on the first row otherwise.
    pub(super) fn showing(
        target: Target,
        prs: Vec<PrEntry>,
        detected: Option<&PrIdentity>,
    ) -> Self {
        let selected = detected
            .and_then(|identity| prs.iter().position(|pr| &pr.identity == identity))
            .unwrap_or(0);
        Self {
            target,
            prs,
            selected,
            error: None,
            filter: PrFilter::All,
        }
    }

    /// Modal reporting why the inventory could not be read.
    fn failed(target: Target, error: SafeError) -> Self {
        Self {
            target,
            prs: Vec::new(),
            selected: 0,
            error: Some(error),
            filter: PrFilter::All,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 表示中の PR 一覧。素材未着なら空。
    #[must_use]
    pub fn prs(&self) -> &[PrEntry] {
        &self.prs
    }
    /// 選択中の添字。
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }
    /// 選択中の PR。一覧が空なら `None`。
    #[must_use]
    pub fn selected_pr(&self) -> Option<&PrEntry> {
        self.prs.get(self.selected)
    }
    /// port が分類した安全なエラー。
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
    #[must_use]
    pub const fn filter(&self) -> PrFilter {
        self.filter
    }
}
/// Reduce one daemon inventory snapshot for `target`.
pub(super) fn absorb(state: &mut AppState, target: Target, revision: u64, prs: &[PrEntry]) {
    let detected = match absorb_pr_snapshot(state, target, revision, prs) {
        // A stale snapshot proves nothing, so it must not rebuild the modal's
        // rows or clear the error the last real answer left there. It still
        // answers the request that provoked it.
        PrSnapshot::Ignored => None,
        PrSnapshot::Absorbed(detected) => {
            refresh_pr_modal(state, target, detected.as_ref());
            detected
        }
    };
    settle_pr_request(state, target, detected.as_ref());
    announce_detected_pr(state, target, detected.as_ref());
}

/// Report why the inventory for `target` could not be read.
///
/// An open modal keeps its rows and says they may be stale; a request that has
/// no modal yet becomes one, so an explicit `p` never fails silently.
pub(super) fn fail(state: &mut AppState, target: Target, error: &SafeError) {
    if let Some(overlay) = state
        .pr_overlay
        .as_mut()
        .filter(|overlay| overlay.target == target)
    {
        overlay.error = Some(error.clone());
    } else if state.pr_request == Some(target) {
        state.pr_request = None;
        if state.can_surface_pr_modal() {
            state.open_pr_modal(PrOverlay::failed(target, error.clone()));
        }
    }
}

/// Drop the inventory, modal, and request of every target the workspace
/// snapshot no longer contains, so a later session cannot inherit their rows.
pub(super) fn forget_untracked_targets(state: &mut AppState) {
    let before = state.prs.len();
    state
        .prs
        .retain(|target, _| is_tracked(&state.sessions, *target));
    if state.prs.len() != before {
        state.session_pr_revision = state.session_pr_revision.saturating_add(1);
    }
    if state
        .pr_overlay
        .as_ref()
        .is_some_and(|overlay| !is_tracked(&state.sessions, overlay.target))
    {
        state.close_pr_modal();
    }
    if state
        .pr_request
        .is_some_and(|target| !is_tracked(&state.sessions, target))
    {
        state.pr_request = None;
    }
}

/// What one daemon snapshot did to the cached inventory.
enum PrSnapshot {
    /// Stale or untracked: the inventory keeps what the last real answer left.
    Ignored,
    /// Cached, carrying the PR whose first appearance the user is waiting for.
    Absorbed(Option<PrIdentity>),
}

/// Cache one daemon snapshot and report the PR whose first appearance the user
/// is waiting for.
///
/// A snapshot is authoritative only while it is newer than the cached revision
/// and its target is still tracked; anything else leaves the inventory alone.
/// The first snapshot of a target establishes the baseline, so only a URL added
/// by a later revision counts as a live discovery.
fn absorb_pr_snapshot(
    state: &mut AppState,
    target: Target,
    revision: u64,
    prs: &[PrEntry],
) -> PrSnapshot {
    let known = state.prs.get(&target);
    if !is_tracked(&state.sessions, target)
        || known.is_some_and(|(current, _)| revision <= *current)
    {
        return PrSnapshot::Ignored;
    }
    let detected = known
        .and_then(|(_, known)| {
            prs.iter().find(|pr| {
                pr.is_visible()
                    && pr.auto_open
                    && known.iter().all(|known| known.identity != pr.identity)
            })
        })
        .map(|pr| pr.identity.clone());
    let newly_merged = known.is_some_and(|(_, known)| {
        prs.iter().any(|pr| {
            pr.state == PrState::Merged
                && known
                    .iter()
                    .any(|known| known.identity == pr.identity && known.state != PrState::Merged)
        })
    });
    if let (true, Some(session)) = (newly_merged, target.session_id()) {
        state
            .pr_merge_celebrations
            .insert(session, state.mascot_tick.saturating_add(24));
    }
    state.prs.insert(target, (revision, prs.to_vec()));
    state.session_pr_revision = state.session_pr_revision.saturating_add(1);
    PrSnapshot::Absorbed(detected)
}

/// Rebuild the open modal from the refreshed inventory of its own target.
///
/// An inventory with no visible PR at all closes the modal. A status tab with
/// no matches keeps it open, so the user can move to another tab instead of
/// reopening the inventory.
fn refresh_pr_modal(state: &mut AppState, target: Target, detected: Option<&PrIdentity>) {
    let Some((filter, previous)) = state
        .pr_overlay
        .as_ref()
        .filter(|overlay| overlay.target == target)
        .map(|overlay| (overlay.filter, overlay.selected))
    else {
        return;
    };
    let inventory = state.prs_for(target).unwrap_or_default();
    if !inventory.iter().any(PrEntry::is_visible) {
        state.close_pr_modal();
        return;
    }
    let rows = filtered_prs(inventory, filter);
    let selected = detected
        .and_then(|identity| rows.iter().position(|pr| &pr.identity == identity))
        .unwrap_or(previous)
        .min(rows.len().saturating_sub(1));
    if let Some(overlay) = state.pr_overlay.as_mut() {
        overlay.prs = rows;
        overlay.selected = selected;
        overlay.error = None;
    }
}

/// Answer the explicit `p` request for `target` with the snapshot it asked for.
///
/// The modal appears only when the inventory has something to show and nothing
/// else has taken the foreground meanwhile; a delayed answer never displaces a
/// newer interaction. Either way the request is spent.
fn settle_pr_request(state: &mut AppState, target: Target, detected: Option<&PrIdentity>) {
    if state.pr_request != Some(target) {
        return;
    }
    state.pr_request = None;
    let visible = state.visible_prs(target);
    if visible.is_empty() || !state.can_surface_pr_modal() {
        return;
    }
    state.open_pr_modal(PrOverlay::showing(target, visible, detected));
}

/// Surface a PR the user has not seen before: it is the completion of work they
/// are waiting for. Metadata-only refreshes, duplicate snapshots, and deliberate
/// dismissals stay quiet, and an open modal or Director interaction remains the
/// input owner.
fn announce_detected_pr(state: &mut AppState, target: Target, detected: Option<&PrIdentity>) {
    let Some(identity) = detected else {
        return;
    };
    let may_open = match state.pr_auto_open {
        PrAutoOpen::Always => true,
        PrAutoOpen::SwitchOnly => matches!(state.route, Route::Home(HomeMode::Switch)),
        PrAutoOpen::NotifyOnly | PrAutoOpen::Never => false,
    };
    if may_open && state.can_surface_pr_modal() {
        let visible = state.visible_prs(target);
        state.open_pr_modal(PrOverlay::showing(target, visible, Some(identity)));
    } else if state.pr_auto_open == PrAutoOpen::NotifyOnly {
        state.notice = Some(Notice::new(format!("PR detected: {}", identity.as_url())));
    }
}

/// Whether the workspace snapshot still contains this target. The workspace
/// root outlives every session, so only a session can stop being tracked.
fn is_tracked(sessions: &[SessionId], target: Target) -> bool {
    target
        .session_id()
        .is_none_or(|session| sessions.contains(&session))
}

/// The rows one status tab shows, in inventory order.
fn filtered_prs(prs: &[PrEntry], filter: PrFilter) -> Vec<PrEntry> {
    prs.iter()
        .filter(|pr| pr.is_visible() && filter.admits(pr.state))
        .cloned()
        .collect()
}
/// Open the modal for the active target, if Home has one.
pub(super) fn open(state: &mut AppState) -> Vec<Effect> {
    let Some(target) = state.active_target() else {
        return Vec::new();
    };
    open_for(state, target)
}

/// Open the PR modal for `target` on what the cache already knows, and always
/// ask the daemon for a fresh inventory.
///
/// With nothing cached there is nothing to show, so the request is recorded
/// instead of a modal: an empty box that answers no key is worse than no box.
/// [`settle_pr_request`] opens it when the snapshot proves there is something
/// in it.
pub(super) fn open_for(state: &mut AppState, target: Target) -> Vec<Effect> {
    let visible = state.visible_prs(target);
    // The request dismisses the surface it was asked from: the answer belongs in
    // the foreground the user just released, and anything opened after this
    // point is newer than the request and keeps it.
    state.overlay = None;
    if visible.is_empty() {
        state.pr_request = Some(target);
    } else {
        state.open_pr_modal(PrOverlay::showing(target, visible, None));
    }
    vec![Effect::LoadPullRequests { target }]
}
/// Pull Request overlay の入力を還元する。←→ で status tab、↑↓ で PR 選択を回し、
/// Enter で選択 PR を browser で開く effect を出す。Esc は overlay を閉じる。
/// 素材の再取得はしない。
pub(super) fn update_key(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    if matches!(key, AppKey::Left | AppKey::Right) {
        let Some((target, filter)) = state.pr_overlay.as_ref().map(|overlay| {
            let filter = if matches!(key, AppKey::Right) {
                overlay.filter.next()
            } else {
                overlay.filter.previous()
            };
            (overlay.target, filter)
        }) else {
            return Vec::new();
        };
        let all = state.prs_for(target).unwrap_or_default().to_vec();
        if let Some(overlay) = state.pr_overlay.as_mut() {
            overlay.filter = filter;
            overlay.prs = filtered_prs(&all, filter);
            overlay.selected = 0;
        }
        return Vec::new();
    }
    let Some(overlay) = state.pr_overlay.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.close_pr_modal();
            Vec::new()
        }
        AppKey::Up => {
            if !overlay.prs.is_empty() {
                overlay.selected = (overlay.selected + overlay.prs.len() - 1) % overlay.prs.len();
            }
            Vec::new()
        }
        AppKey::Down => {
            if !overlay.prs.is_empty() {
                overlay.selected = (overlay.selected + 1) % overlay.prs.len();
            }
            Vec::new()
        }
        AppKey::Enter => overlay
            .selected_pr()
            .map(|pr| Effect::OpenPullRequest {
                url: pr.url().to_owned(),
            })
            .into_iter()
            .collect(),
        AppKey::Char('c') => overlay
            .selected_pr()
            .map(|pr| Effect::CopyPullRequest {
                url: pr.url().to_owned(),
            })
            .into_iter()
            .collect(),
        AppKey::CtrlX => overlay
            .selected_pr()
            .and_then(|pr| {
                overlay
                    .target
                    .session_id()
                    .map(|session| Effect::DismissPullRequest {
                        session,
                        url: pr.url().to_owned(),
                    })
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}
