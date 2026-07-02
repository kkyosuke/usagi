//! The selectable worktree list (left pane): the workspace's sessions collapsed
//! into one row each, preceded by the synthetic root row.
//!
//! The list is modelled as a sequence of [`WorkspaceGroup`]s — one per opened
//! workspace — so the future *unite* mode (several workspaces shown together)
//! reuses the same navigation. Each group contributes a synthetic root row
//! followed by one row per session. Today the home screen opens a single
//! workspace, so the list holds exactly one group and behaves identically to the
//! old single-workspace list; the multi-group machinery is exercised by the unit
//! tests until a producer (the Open screen's multi-select) lands.
//!
//! Navigation runs over a *flat selectable-row space* that concatenates every
//! group's rows: group 0's root, then its worktrees, then group 1's root, and so
//! on. The cursor (`selected_index`) and the command target (`active_index`) are
//! indices into that flat space.

use std::path::Path;

use crate::domain::workspace_state::{
    AheadBehind, BranchStatus, DiffStat, PrLink, SessionRecord, WorktreeState,
};

use super::super::command::WorktreeRef;

/// The display name of a worktree: its branch, or a placeholder when detached.
pub fn worktree_name(worktree: &WorktreeState) -> &str {
    worktree.branch.as_deref().unwrap_or("(detached)")
}

/// The name of the root row: the workspace itself, belonging to no session.
/// Used as its display label and to target it from `session switch root`.
pub const ROOT_NAME: &str = "root";

/// Collapse a session's per-repository worktrees into the single list row that
/// represents it.
///
/// A session is one branch name checked out into a worktree in every repository
/// under the workspace, so the home list shows one row per *session*, not per
/// repository. The row is keyed on the session tree root (`<workspace>/.usagi/
/// sessions/<name>`) — where the embedded terminal/agent roots and the key for
/// its live/waiting state — and carries:
///
/// - the session name as its branch label,
/// - a status [aggregated](BranchStatus::aggregate) across the repositories (the
///   least-progressed, so `synced` means every repository's branch has landed),
/// - `primary` set when any repository's worktree is the primary checkout,
/// - the first repository's `head` / `upstream` as representative detail,
/// - `updated_at` carrying the session's last-active time (or its creation time
///   when never touched), which the sidebar reads for both the freshness ("heat")
///   dot and the line-2 `Nmin ago` label.
///
/// For a single-repository workspace the session root *is* that repository's
/// worktree, so the row matches the lone worktree exactly.
pub(super) fn session_row(session: &SessionRecord) -> WorktreeState {
    let status = BranchStatus::aggregate(session.worktrees.iter().map(|w| w.status));
    let primary = session.worktrees.iter().any(|w| w.primary);
    let first = session.worktrees.first();
    WorktreeState {
        branch: Some(session.name.clone()),
        path: session.root.clone(),
        head: first.map(|w| w.head.clone()).unwrap_or_default(),
        primary,
        upstream: first.and_then(|w| w.upstream.clone()),
        status,
        diff: DiffStat::aggregate(session.worktrees.iter().map(|w| w.diff)),
        ahead_behind: AheadBehind::aggregate(session.worktrees.iter().map(|w| w.ahead_behind)),
        pr: PrLink::aggregate(session.worktrees.iter().flat_map(|w| w.pr.iter().cloned())),
        // Both the heat dot and the line-2 `Nmin ago` label fade by time since the
        // session was last touched (switched to, or seen active), falling back to
        // its creation time when it never has been. Unlike each worktree's git-sync
        // `updated_at` — reset for every session on each workspace sync — this is a
        // per-session signal, so it tells the live sessions from the stale ones.
        updated_at: session.last_active_or_created(),
    }
}

/// One opened workspace's slice of the left pane: its sessions (collapsed to one
/// [`WorktreeState`] row each by [`session_row`]) plus the per-row sidebar
/// metadata, fronted in the list by a synthetic root row the group owns.
///
/// In single-workspace mode the [`WorktreeList`] holds one of these; *unite* mode
/// holds several, stacked under per-workspace headers.
#[derive(Debug, Clone)]
pub struct WorkspaceGroup {
    name: String,
    worktrees: Vec<WorktreeState>,
    /// Sidebar label overrides, aligned 1:1 with `worktrees`: `labels[i]` is the
    /// custom display name for `worktrees[i]`, or `None` to show its branch. The
    /// override is cosmetic only — every name-based lookup keys on the branch (the
    /// session identity), never on the label.
    labels: Vec<Option<String>>,
    /// Whether each row's session carries a note, aligned 1:1 with `worktrees`
    /// (`notes[i]` is for `worktrees[i]`). Drives the line-1 memo marker; like
    /// `labels` it is cosmetic and never used for lookups. Defaults to all-false
    /// and is filled in by [`set_notes`](Self::set_notes) on a list rebuild.
    notes: Vec<bool>,
    /// The manual-status label id each row's session carries, aligned 1:1 with
    /// `worktrees` (`label_ids[i]` is for `worktrees[i]`), or `None` when unset.
    /// Resolved against the effective label master by the renderer; like `labels`
    /// / `notes` it is cosmetic and never used for lookups. Defaults to all-`None`
    /// and is filled in by [`set_label_ids`](Self::set_label_ids) on a rebuild.
    label_ids: Vec<Option<String>>,
    /// Whether this group's synthetic root row carries a note, driving its line-1
    /// memo marker. Like [`notes`](Self::notes) it is cosmetic and never used for
    /// lookups; the root belongs to no session, so its note lives on the workspace
    /// state, not in `notes`.
    root_has_note: bool,
}

impl WorkspaceGroup {
    /// A group for `name` with no label overrides and no notes yet.
    pub fn new(name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        let labels = vec![None; worktrees.len()];
        Self::with_labels(name, worktrees, labels)
    }

    /// Build a group's rows from a workspace's recorded sessions — the same
    /// collapse [`HomeState::rebuild_list`] does for the primary workspace: one row
    /// per session via [`session_row`], carrying each session's display-name label
    /// and note marker, plus whether the workspace root itself carries a note.
    /// Used by the orchestrator to build the extra 統合(unite) groups.
    ///
    /// [`HomeState::rebuild_list`]: super::HomeState
    pub fn from_sessions(
        name: impl Into<String>,
        sessions: &[SessionRecord],
        root_has_note: bool,
    ) -> Self {
        let rows = sessions.iter().map(session_row).collect();
        let labels = sessions.iter().map(|s| s.display_name.clone()).collect();
        let notes = sessions.iter().map(|s| s.note.is_some()).collect();
        let label_ids = sessions.iter().map(|s| s.label_id.clone()).collect();
        let mut group = Self::with_labels(name, rows, labels);
        group.set_notes(notes);
        group.set_label_ids(label_ids);
        group.set_root_note_marker(root_has_note);
        group
    }

    /// A group with a sidebar label override per worktree (`labels[i]` applies to
    /// `worktrees[i]`; a shorter/longer `labels` is padded/truncated to match).
    pub fn with_labels(
        name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        mut labels: Vec<Option<String>>,
    ) -> Self {
        labels.resize(worktrees.len(), None);
        let notes = vec![false; worktrees.len()];
        let label_ids = vec![None; worktrees.len()];
        Self {
            name: name.into(),
            worktrees,
            labels,
            notes,
            label_ids,
            root_has_note: false,
        }
    }

    /// The workspace name shown in this group's header / title bar.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The group's session rows (one per session, root row excluded).
    pub fn worktrees(&self) -> &[WorktreeState] {
        &self.worktrees
    }

    /// The sidebar label for the worktree at `index`: its override when set,
    /// otherwise its branch name (the same string [`worktree_name`] returns).
    pub fn display_label(&self, index: usize) -> &str {
        match self.labels.get(index).and_then(Option::as_deref) {
            Some(label) => label,
            None => self.worktrees.get(index).map(worktree_name).unwrap_or(""),
        }
    }

    /// Record which rows carry a note (`notes[i]` for `worktrees[i]`). A
    /// shorter/longer slice is padded/truncated to the worktree count, mirroring
    /// how [`with_labels`](Self::with_labels) keeps `labels` aligned.
    pub fn set_notes(&mut self, mut notes: Vec<bool>) {
        notes.resize(self.worktrees.len(), false);
        self.notes = notes;
    }

    /// Whether the worktree at `index` carries a note (out-of-range is `false`).
    pub fn has_note(&self, index: usize) -> bool {
        self.notes.get(index).copied().unwrap_or(false)
    }

    /// Record each row's manual-status label id (`label_ids[i]` for
    /// `worktrees[i]`). A shorter/longer slice is padded/truncated to the worktree
    /// count, mirroring [`set_notes`](Self::set_notes).
    pub fn set_label_ids(&mut self, mut label_ids: Vec<Option<String>>) {
        label_ids.resize(self.worktrees.len(), None);
        self.label_ids = label_ids;
    }

    /// The manual-status label id the worktree at `index` carries, or `None` when
    /// unset / out of range. Resolved against the effective master by the renderer.
    pub fn row_label_id(&self, index: usize) -> Option<&str> {
        self.label_ids.get(index).and_then(Option::as_deref)
    }

    /// Record whether the root row carries a note, driving its memo marker.
    pub fn set_root_note_marker(&mut self, has_note: bool) {
        self.root_has_note = has_note;
    }

    /// Whether the root row carries a note (drives its memo marker).
    pub fn root_has_note(&self) -> bool {
        self.root_has_note
    }

    /// Number of selectable rows this group contributes: its root row plus every
    /// worktree (≥ 1).
    fn selectable_rows(&self) -> usize {
        self.worktrees.len() + 1
    }
}

/// The opened workspace(s) and their selectable rows, each group fronted by a
/// synthetic *root row*.
///
/// Within a group, the first row is the workspace root, which belongs to no
/// session: activating it and running `terminal`/`agent` there works at the
/// workspace root rather than inside a session's worktree. Navigation runs over a
/// flat row space concatenating every group's rows.
///
/// Two cursors are tracked: `selected_index` is where the keyboard cursor sits
/// while navigating, and `active_index` is the row subsequent commands
/// (`session switch`, `terminal`/`agent`) act on. Both default to the first
/// group's root row.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    /// The opened workspaces, in display order. Always non-empty in practice (the
    /// home screen opens at least one workspace).
    groups: Vec<WorkspaceGroup>,
    selected_index: usize,
    active_index: usize,
    /// The display name of the session that was active *before* the current one,
    /// or `None` until the active row has moved off its initial spot. It is the
    /// target `Ctrl-^` jumps back to (vim's `Ctrl-^` / tmux's `last-window`):
    /// recorded by [`activate_selected`](Self::activate_selected) whenever the
    /// active row changes to a *different* session, and resolved back to a current
    /// row by [`previous_row`](Self::previous_row). Stored as a name, not an index,
    /// so a list rebuild (a background re-sync) keeps it pointing at the same
    /// session — and drops it when that session is gone.
    previous_active: Option<String>,
}

impl WorktreeList {
    /// Builds a single-group list for the named workspace, with both the cursor
    /// and the active row on the root (no session selected yet) and no label
    /// overrides.
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self::from_groups(vec![WorkspaceGroup::new(workspace_name, worktrees)])
    }

    /// Builds a single-group list with a sidebar label override per worktree
    /// (`labels[i]` applies to `worktrees[i]`; a shorter/longer `labels` is
    /// padded/ignored to match), with both cursors on the root row.
    pub fn with_labels(
        workspace_name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        labels: Vec<Option<String>>,
    ) -> Self {
        Self::from_groups(vec![WorkspaceGroup::with_labels(
            workspace_name,
            worktrees,
            labels,
        )])
    }

    /// Builds a list from one or more workspace groups (unite mode supplies
    /// several), with both cursors on the first group's root row.
    pub fn from_groups(groups: Vec<WorkspaceGroup>) -> Self {
        Self {
            groups,
            selected_index: 0,
            active_index: 0,
            previous_active: None,
        }
    }

    /// Append another workspace group (unite mode) and return its index.
    pub fn add_group(&mut self, group: WorkspaceGroup) -> usize {
        self.groups.push(group);
        self.groups.len().saturating_sub(1)
    }

    /// The workspace groups, in display order.
    pub fn groups(&self) -> &[WorkspaceGroup] {
        &self.groups
    }

    /// Number of workspace groups (1 in single-workspace mode).
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// The first group, the one the legacy single-workspace accessors delegate to.
    fn first(&self) -> Option<&WorkspaceGroup> {
        self.groups.first()
    }

    // --- legacy single-workspace accessors (delegate to the first group) ---

    pub fn workspace_name(&self) -> &str {
        self.first().map(WorkspaceGroup::name).unwrap_or("")
    }

    pub fn worktrees(&self) -> &[WorktreeState] {
        self.first().map(WorkspaceGroup::worktrees).unwrap_or(&[])
    }

    /// The sidebar label for the worktree at `index` in the first group.
    pub fn display_label(&self, index: usize) -> &str {
        self.first().map(|g| g.display_label(index)).unwrap_or("")
    }

    /// Record the first group's per-worktree notes (see
    /// [`WorkspaceGroup::set_notes`]).
    pub fn set_notes(&mut self, notes: Vec<bool>) {
        if let Some(group) = self.groups.first_mut() {
            group.set_notes(notes);
        }
    }

    /// Whether the worktree at `index` in the first group carries a note.
    pub fn has_note(&self, index: usize) -> bool {
        self.first().map(|g| g.has_note(index)).unwrap_or(false)
    }

    /// Record the first group's per-worktree manual-status label ids (see
    /// [`WorkspaceGroup::set_label_ids`]).
    pub fn set_label_ids(&mut self, label_ids: Vec<Option<String>>) {
        if let Some(group) = self.groups.first_mut() {
            group.set_label_ids(label_ids);
        }
    }

    /// The manual-status label id the worktree at `index` in the first group
    /// carries, or `None` when unset (see [`WorkspaceGroup::row_label_id`]).
    pub fn row_label_id(&self, index: usize) -> Option<&str> {
        self.first().and_then(|g| g.row_label_id(index))
    }

    /// Record whether the first group's root row carries a note.
    pub fn set_root_note_marker(&mut self, has_note: bool) {
        if let Some(group) = self.groups.first_mut() {
            group.set_root_note_marker(has_note);
        }
    }

    /// Whether the first group's root row carries a note.
    pub fn root_has_note(&self) -> bool {
        self.first()
            .map(WorkspaceGroup::root_has_note)
            .unwrap_or(false)
    }

    // --- flat row-space navigation (spans every group) ---

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Index of the active row (the one commands act on).
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Whether no group has any recorded worktrees (only root rows).
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.worktrees.is_empty())
    }

    /// Total selectable rows across every group: each group's root row plus its
    /// worktrees.
    fn selectable_rows(&self) -> usize {
        self.groups
            .iter()
            .map(WorkspaceGroup::selectable_rows)
            .sum()
    }

    /// Total rows the 切替 cursor can rest on: every selectable session / root row
    /// plus the trailing persistent **"+ new session"** row at the foot of the
    /// list. Navigation ([`move_up`](Self::move_up) / [`move_down`](Self::move_down)
    /// / [`focus_index`](Self::focus_index)) wraps over this range so the cursor can
    /// land on the create row, while the session-facing accessors (`selected`,
    /// `refs`, the title-bar [`session_count`](Self::session_count)) keep counting
    /// only the real rows.
    fn nav_rows(&self) -> usize {
        self.selectable_rows() + 1
    }

    /// The flat index of the persistent **"+ new session"** row — the last one the
    /// cursor can reach, sitting just past every group's rows. It belongs to no
    /// session (so [`selected`](Self::selected) / [`root_selected`](Self::root_selected)
    /// stay `false`/`None` on it); activating it opens the inline create input
    /// instead of focusing a session.
    pub fn create_row(&self) -> usize {
        self.selectable_rows()
    }

    /// Whether the cursor rests on the persistent **"+ new session"** row (see
    /// [`create_row`](Self::create_row)).
    pub fn create_row_selected(&self) -> bool {
        self.selected_index == self.create_row()
    }

    /// Number of rows listed in the left pane (every group's root row plus its
    /// worktrees). The title bar reports this so the header count matches the rows
    /// the user actually sees.
    pub fn session_count(&self) -> usize {
        self.selectable_rows()
    }

    /// Resolve a flat selectable `row` to `(group index, worktree index within the
    /// group)`, where the worktree index is `None` for that group's root row.
    /// `None` when `row` is past the end.
    fn locate(&self, row: usize) -> Option<(usize, Option<usize>)> {
        let mut start = 0;
        self.groups.iter().enumerate().find_map(|(g, group)| {
            let end = start + group.selectable_rows();
            let found = (start..end)
                .contains(&row)
                .then(|| (g, (row - start).checked_sub(1)));
            start = end;
            found
        })
    }

    /// The worktree at a selectable `row`, or `None` when the row is a group's
    /// root row (which belongs to no session) or past the end.
    fn worktree_at(&self, row: usize) -> Option<&WorktreeState> {
        let (g, within) = self.locate(row)?;
        self.groups[g].worktrees.get(within?)
    }

    /// The worktree at a global 0-based index across every group (root rows
    /// excluded), or `None` when out of range. The PR popup keys off this so it
    /// can pin a session's badge in any workspace, not just the first group.
    pub fn worktree_by_global_index(&self, idx: usize) -> Option<&WorktreeState> {
        self.groups.iter().flat_map(|g| g.worktrees()).nth(idx)
    }

    /// The worktree under the cursor, or `None` when the cursor is on a root row.
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.selected_index)
    }

    /// The active worktree, or `None` when a root row is active.
    pub fn active(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.active_index)
    }

    /// The group the cursor currently sits in (0 in single-workspace mode).
    pub fn selected_group(&self) -> usize {
        self.locate(self.selected_index)
            .map(|(g, _)| g)
            .unwrap_or(0)
    }

    /// The group the active row sits in (0 in single-workspace mode).
    pub fn active_group(&self) -> usize {
        self.locate(self.active_index).map(|(g, _)| g).unwrap_or(0)
    }

    /// Replaces the PR links of the row whose session root is `root`, returning
    /// whether the set changed.
    ///
    /// Lets an attached pane reflect a freshly detected pull-request URL in the
    /// sidebar `#N` badge immediately, instead of waiting for the next workspace
    /// re-sync (a slow, per-worktree `git status`) to fold `pr-links/` into
    /// `state.json`. The caller passes the store's accumulated, deduped set — the
    /// same value the re-sync would compute — so the live badge matches what a
    /// later sync produces. A `root` that matches no row (e.g. the workspace root,
    /// which has no worktree) is a no-op.
    pub fn set_pr_links(&mut self, root: &Path, prs: Vec<PrLink>) -> bool {
        let Some(wt) = self
            .groups
            .iter_mut()
            .flat_map(|g| g.worktrees.iter_mut())
            .find(|w| w.path.as_path() == root)
        else {
            return false;
        };
        if wt.pr == prs {
            return false;
        }
        wt.pr = prs;
        true
    }

    /// Whether the cursor is on a (any group's) root row.
    pub fn root_selected(&self) -> bool {
        !self.create_row_selected() && matches!(self.locate(self.selected_index), Some((_, None)))
    }

    /// Whether a root row is the active one.
    pub fn root_active(&self) -> bool {
        matches!(self.locate(self.active_index), Some((_, None)))
    }

    /// Make the row under the cursor active, returning its display name (the
    /// branch name, or [`ROOT_NAME`] for a root row).
    ///
    /// When this lands on a *different* session than the one currently active,
    /// the one being left is remembered as the [`previous_row`](Self::previous_row)
    /// `Ctrl-^` jumps back to; re-activating the same row leaves that memory
    /// untouched (so a no-op focus does not erase where to jump back to).
    pub fn activate_selected(&mut self) -> &str {
        if self.selected_index != self.active_index {
            self.previous_active = Some(self.active_name().to_string());
        }
        self.active_index = self.selected_index;
        self.active_name()
    }

    /// The name of the previously active session, for carrying the `Ctrl-^` jump
    /// target across a list rebuild (a background re-sync drops the list and
    /// builds a fresh one). Paired with [`set_previous_active`](Self::set_previous_active).
    pub fn previous_active_name(&self) -> Option<&str> {
        self.previous_active.as_deref()
    }

    /// Restore the previously active session after a rebuild, so the `Ctrl-^`
    /// jump survives a background re-sync. The name is validated lazily by
    /// [`previous_row`](Self::previous_row), so one that no longer matches simply
    /// yields no jump rather than an error.
    pub fn set_previous_active(&mut self, name: Option<String>) {
        self.previous_active = name;
    }

    /// The flat row the previously active session now sits at (a group's root row
    /// for [`ROOT_NAME`]), or `None` when no previous session has been recorded
    /// yet or it has since been removed from the list. Resolved by name so a list
    /// rebuild keeps it pointing at the same session — the target `Ctrl-^` focuses.
    pub fn previous_row(&self) -> Option<usize> {
        let name = self.previous_active.as_deref()?;
        (0..self.selectable_rows()).find(|&row| match self.worktree_at(row) {
            Some(w) => worktree_name(w) == name,
            None => name == ROOT_NAME,
        })
    }

    /// The display name of the active row: its branch, or [`ROOT_NAME`] for a
    /// root row.
    pub fn active_name(&self) -> &str {
        self.active().map(worktree_name).unwrap_or(ROOT_NAME)
    }

    /// The display name of the row under the cursor: its branch, or [`ROOT_NAME`]
    /// for a root row.
    pub fn selected_name(&self) -> &str {
        self.selected().map(worktree_name).unwrap_or(ROOT_NAME)
    }

    /// Make the row named `name` active, returning whether one matched.
    /// [`ROOT_NAME`] activates the first group's root row; every other name is
    /// matched against the worktree branches (the first match across groups).
    pub fn activate_by_name(&mut self, name: &str) -> bool {
        if name == ROOT_NAME {
            self.active_index = 0;
            return true;
        }
        match self.row_of_name(name) {
            Some(row) => {
                self.active_index = row;
                true
            }
            None => false,
        }
    }

    /// Move both the cursor and the active row onto the first worktree named
    /// `name` (a freshly created session's branch), returning whether one
    /// matched. Used after creating a session so the new one is selected and
    /// active without the user navigating to it.
    pub fn select_by_name(&mut self, name: &str) -> bool {
        match self.row_of_name(name) {
            Some(row) => {
                self.selected_index = row;
                self.active_index = row;
                true
            }
            None => false,
        }
    }

    /// The flat row of the first worktree named `name` across all groups.
    fn row_of_name(&self, name: &str) -> Option<usize> {
        (0..self.selectable_rows()).find(|&row| {
            self.worktree_at(row)
                .is_some_and(|w| worktree_name(w) == name)
        })
    }

    /// The rows as command-facing [`WorktreeRef`]s (name + active flag): each
    /// group's root row, then its worktrees, in display order.
    pub fn refs(&self) -> Vec<WorktreeRef> {
        let mut refs = Vec::with_capacity(self.selectable_rows());
        let mut row = 0;
        for group in &self.groups {
            refs.push(WorktreeRef {
                name: ROOT_NAME.to_string(),
                active: row == self.active_index,
            });
            row += 1;
            for w in &group.worktrees {
                refs.push(WorktreeRef {
                    name: worktree_name(w).to_string(),
                    active: row == self.active_index,
                });
                row += 1;
            }
        }
        refs
    }

    /// Move the cursor directly to a flat selectable `row`, clamped to the rows
    /// that exist. Used by the session picker (`Ctrl-O`) to jump straight to the
    /// chosen session.
    pub fn focus_index(&mut self, row: usize) {
        self.selected_index = row.min(self.nav_rows().saturating_sub(1));
    }

    /// Move the cursor up one row, wrapping from the top to the bottom.
    pub fn move_up(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.nav_rows() - 1);
    }

    /// Move the cursor down one row, wrapping from the bottom to the top.
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.nav_rows();
    }
}
