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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
///   dot and the line-2 `Nm ago` label.
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
        // Both the heat dot and the line-2 `Nm ago` label fade by time since the
        // session was last touched (switched to, or seen active), falling back to
        // its creation time when it never has been. Unlike each worktree's git-sync
        // `updated_at` — reset for every session on each workspace sync — this is a
        // per-session signal, so it tells the live sessions from the stale ones.
        updated_at: session.last_active_or_created(),
    }
}

/// The display order and nesting depth of `sessions`, derived from each
/// [`SessionRecord::started_from`] parent link.
///
/// `base_order` is the caller's preferred flat order (manual order, or the
/// waiting-first projection). Root sessions keep that order, and each parent's
/// children are inserted immediately below it, preserving their relative
/// `base_order` among siblings. A session whose parent is missing is left at the
/// root level: there is no visible parent to nest it under.
pub(super) fn session_tree_layout(
    sessions: &[SessionRecord],
    base_order: &[usize],
) -> Vec<(usize, usize)> {
    let name_to_index: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .map(|(i, session)| (session.name.as_str(), i))
        .collect();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut child_indices = HashSet::new();
    for &i in base_order {
        let Some(session) = sessions.get(i) else {
            continue;
        };
        let Some(parent) = session.started_from.as_deref() else {
            continue;
        };
        let Some(&parent_index) = name_to_index.get(parent) else {
            continue;
        };
        if parent_index == i {
            // A self-parented record is malformed; draw it as a root so the list
            // remains navigable and the DFS below cannot loop on it.
            continue;
        }
        children.entry(parent).or_default().push(i);
        child_indices.insert(i);
    }

    let mut layout = Vec::with_capacity(sessions.len());
    let mut visited = HashSet::new();
    fn push_tree<'a>(
        index: usize,
        depth: usize,
        sessions: &'a [SessionRecord],
        children: &HashMap<&'a str, Vec<usize>>,
        visited: &mut HashSet<usize>,
        layout: &mut Vec<(usize, usize)>,
    ) {
        if !visited.insert(index) {
            return;
        }
        layout.push((index, depth));
        let session = &sessions[index];
        if let Some(child_rows) = children.get(session.name.as_str()) {
            for &child in child_rows {
                push_tree(child, depth + 1, sessions, children, visited, layout);
            }
        }
    }

    for &i in base_order {
        if i >= sessions.len() || child_indices.contains(&i) {
            continue;
        }
        push_tree(i, 0, sessions, &children, &mut visited, &mut layout);
    }
    // Cycles (A started_from B, B started_from A) produce no root. Fall back to
    // the caller's flat order for any still-unvisited records, then let the DFS
    // show reachable descendants beneath that first recovered root.
    for &i in base_order {
        if i < sessions.len() && !visited.contains(&i) {
            push_tree(i, 0, sessions, &children, &mut visited, &mut layout);
        }
    }
    layout
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
    /// The workspace root directory — the `⌂ root` row's working dir, and the
    /// target `session` commands run against when the cursor is in this group. It
    /// is the group's identity across the flat row space: a `(root_path, name)`
    /// pair addresses one session unambiguously even when another group holds a
    /// session of the same name. Empty until injected (a single-workspace list
    /// seeds it through [`WorktreeList::set_root_path`]); the extra 統合(unite)
    /// groups carry it from their [`from_sessions`](Self::from_sessions) build.
    root_path: PathBuf,
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
    /// Visual nesting depth for each session row, aligned 1:1 with `worktrees`.
    /// Depth `0` is a root-level session; depth `1+` means this session was
    /// started from a visible parent session and is drawn indented below it.
    nesting_depths: Vec<usize>,
    /// Whether this group's synthetic root row carries a note, driving its line-1
    /// memo marker. Like [`notes`](Self::notes) it is cosmetic and never used for
    /// lookups; the root belongs to no session, so its note lives on the workspace
    /// state, not in `notes`.
    root_has_note: bool,
    /// Whether this workspace is folded shut in the sidebar. When set, only the
    /// group's single collapsed header line is drawn and navigable — its root
    /// entry, session rows and "+ new session" row are hidden — so a 統合(unite)
    /// list of many workspaces can be scanned at a glance. Toggled from the root
    /// row (`Space`); only meaningful in unite mode, where the toggle is exposed.
    collapsed: bool,
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
    /// and note marker, the workspace `root_path`, plus whether the workspace root
    /// itself carries a note. Used by the orchestrator to build the extra
    /// 統合(unite) groups.
    ///
    /// [`HomeState::rebuild_list`]: super::HomeState
    pub fn from_sessions(
        name: impl Into<String>,
        root_path: impl Into<PathBuf>,
        sessions: &[SessionRecord],
        root_has_note: bool,
    ) -> Self {
        let base_order: Vec<_> = (0..sessions.len()).collect();
        let layout = session_tree_layout(sessions, &base_order);
        let rows = layout
            .iter()
            .map(|(i, _)| session_row(&sessions[*i]))
            .collect();
        let labels = layout
            .iter()
            .map(|(i, _)| sessions[*i].display_name.clone())
            .collect();
        let notes = layout
            .iter()
            .map(|(i, _)| sessions[*i].note.is_some())
            .collect();
        let label_ids = layout
            .iter()
            .map(|(i, _)| sessions[*i].label_id.clone())
            .collect();
        let nesting_depths = layout.iter().map(|(_, depth)| *depth).collect();
        let mut group = Self::with_labels(name, rows, labels);
        group.set_root_path(root_path);
        group.set_notes(notes);
        group.set_label_ids(label_ids);
        group.set_nesting_depths(nesting_depths);
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
        let nesting_depths = vec![0; worktrees.len()];
        Self {
            name: name.into(),
            root_path: PathBuf::new(),
            worktrees,
            labels,
            notes,
            label_ids,
            nesting_depths,
            root_has_note: false,
            collapsed: false,
        }
    }

    /// The workspace name shown in this group's header / title bar.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace root directory this group operates in (see
    /// [`root_path`](Self::root_path)).
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Record this group's workspace root directory (the primary group is seeded
    /// through [`WorktreeList::set_root_path`]; extra groups carry it from
    /// [`from_sessions`](Self::from_sessions)).
    pub fn set_root_path(&mut self, root_path: impl Into<PathBuf>) {
        self.root_path = root_path.into();
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

    /// Record the visual nesting depth for each row. A shorter/longer slice is
    /// padded/truncated to the worktree count, mirroring the other per-row
    /// metadata setters.
    pub fn set_nesting_depths(&mut self, mut nesting_depths: Vec<usize>) {
        nesting_depths.resize(self.worktrees.len(), 0);
        self.nesting_depths = nesting_depths;
    }

    /// The visual nesting depth of the worktree at `index` (out-of-range is
    /// root-level). Used only by the sidebar renderer.
    pub fn nesting_depth(&self, index: usize) -> usize {
        self.nesting_depths.get(index).copied().unwrap_or(0)
    }

    /// Record whether the root row carries a note, driving its memo marker.
    pub fn set_root_note_marker(&mut self, has_note: bool) {
        self.root_has_note = has_note;
    }

    /// Whether the root row carries a note (drives its memo marker).
    pub fn root_has_note(&self) -> bool {
        self.root_has_note
    }

    /// Whether this workspace is folded shut (see [`collapsed`](Self::collapsed)).
    pub fn collapsed(&self) -> bool {
        self.collapsed
    }

    /// Number of session rows this group contributes to the title count: its root
    /// row plus every worktree (≥ 1). Independent of [`collapsed`](Self::collapsed)
    /// — folding a workspace hides its rows but does not change how many sessions
    /// the header reports.
    fn session_rows(&self) -> usize {
        self.worktrees.len() + 1
    }

    /// Number of *navigable* rows this group contributes to the flat cursor space.
    /// Expanded: its root row, one per worktree, and the trailing "+ new session"
    /// row. Collapsed: just the single folded header line.
    fn nav_slots(&self) -> usize {
        if self.collapsed {
            1
        } else {
            self.worktrees.len() + 2
        }
    }
}

/// Which kind of row a flat cursor index lands on within its group: the synthetic
/// root row, a session (the worktree at the given index), or the trailing
/// "+ new session" affordance. A collapsed group exposes only [`RowSlot::Root`]
/// (its folded header line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSlot {
    Root,
    Worktree(usize),
    Create,
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
    /// The `(workspace root, session name)` of the row that was active *before* the
    /// current one, or `None` until the active row has moved off its initial spot.
    /// It is the target `Ctrl-^` jumps back to (vim's `Ctrl-^` / tmux's
    /// `last-window`): recorded by [`activate_selected`](Self::activate_selected)
    /// whenever the active row changes to a *different* row, and resolved back to a
    /// current row by [`previous_row`](Self::previous_row). Qualified by the group's
    /// root path — not the bare name — so a same-named session in another
    /// 統合(unite) group is never mistaken for it. Stored by identity, not index, so
    /// a list rebuild (a background re-sync) keeps it pointing at the same session —
    /// and drops it when that session is gone.
    previous_active: Option<(PathBuf, String)>,
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

    /// The primary (first group's) workspace root directory — what the legacy
    /// single-workspace [`HomeState::root_path`] delegates to.
    ///
    /// [`HomeState::root_path`]: super::HomeState::root_path
    pub fn root_path(&self) -> &Path {
        self.first()
            .map(WorkspaceGroup::root_path)
            .unwrap_or(Path::new(""))
    }

    /// Record the primary (first group's) workspace root directory — what
    /// [`HomeState::set_root_path`] delegates to. A no-op on the (never-occurring)
    /// empty list.
    ///
    /// [`HomeState::set_root_path`]: super::HomeState::set_root_path
    pub fn set_root_path(&mut self, root_path: impl Into<PathBuf>) {
        if let Some(group) = self.groups.first_mut() {
            group.set_root_path(root_path);
        }
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

    /// Record the first group's per-worktree nesting depths (see
    /// [`WorkspaceGroup::set_nesting_depths`]).
    pub fn set_nesting_depths(&mut self, nesting_depths: Vec<usize>) {
        if let Some(group) = self.groups.first_mut() {
            group.set_nesting_depths(nesting_depths);
        }
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

    /// Total rows the 選択 cursor can rest on across every group: each expanded
    /// group's root row, one per worktree, and its trailing **"+ new session"**
    /// row — or, for a collapsed group, just its single folded header line.
    /// Navigation ([`move_up`](Self::move_up) / [`move_down`](Self::move_down) /
    /// [`focus_index`](Self::focus_index)) wraps over this range so the cursor can
    /// land on any create row, while the title-bar [`session_count`](Self::session_count)
    /// keeps counting only the real session rows.
    fn nav_rows(&self) -> usize {
        self.groups.iter().map(WorkspaceGroup::nav_slots).sum()
    }

    /// The flat index of the **"+ new session"** row at the foot of the *last*
    /// group — the affordance a click at the very bottom of the list resolves to,
    /// and the upper bound the session picker clamps against. Each group owns its
    /// own create row now ([`is_create_row`](Self::is_create_row)); this names the
    /// last one for the legacy foot-of-list callers.
    pub fn create_row(&self) -> usize {
        self.nav_rows().saturating_sub(1)
    }

    /// Whether the flat `row` is a group's **"+ new session"** row. It belongs to
    /// no session (so [`selected`](Self::selected) / [`root_selected`](Self::root_selected)
    /// stay `None`/`false` on it); activating it opens that group's inline create
    /// input instead of focusing a session.
    pub fn is_create_row(&self, row: usize) -> bool {
        matches!(self.locate(row), Some((_, RowSlot::Create)))
    }

    /// Whether the cursor rests on a **"+ new session"** row (see
    /// [`is_create_row`](Self::is_create_row)).
    pub fn create_row_selected(&self) -> bool {
        self.is_create_row(self.selected_index)
    }

    /// Number of session rows listed (every group's root row plus its worktrees).
    /// The title bar reports this; it counts folded workspaces in full, so folding
    /// one changes what is drawn but not the header total.
    pub fn session_count(&self) -> usize {
        self.groups.iter().map(WorkspaceGroup::session_rows).sum()
    }

    /// Resolve a flat cursor `row` to its group and the kind of row it lands on
    /// within that group (root, a worktree, or the create row). A collapsed group
    /// contributes only its [`RowSlot::Root`] line. `None` when `row` is past the
    /// end.
    fn locate(&self, row: usize) -> Option<(usize, RowSlot)> {
        let mut start = 0;
        for (g, group) in self.groups.iter().enumerate() {
            let n = group.nav_slots();
            if row < start + n {
                let within = row - start;
                let slot = if within == 0 {
                    RowSlot::Root
                } else if within <= group.worktrees.len() {
                    RowSlot::Worktree(within - 1)
                } else {
                    RowSlot::Create
                };
                return Some((g, slot));
            }
            start += n;
        }
        None
    }

    /// The worktree at a flat `row`, or `None` when the row is a group's root row
    /// or create row (neither belongs to a session) or is past the end.
    fn worktree_at(&self, row: usize) -> Option<&WorktreeState> {
        match self.locate(row)? {
            (g, RowSlot::Worktree(i)) => self.groups[g].worktrees.get(i),
            _ => None,
        }
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

    /// The workspace root of the group the active row sits in — the group side of
    /// the `(root, name)` identity [`activate_selected`](Self::activate_selected)
    /// records for the `Ctrl-^` jump-back, so it survives a rebuild that reorders
    /// or drops groups. Empty when the active index is somehow out of range.
    fn active_group_root(&self) -> &Path {
        self.groups
            .get(self.active_group())
            .map(WorkspaceGroup::root_path)
            .unwrap_or(Path::new(""))
    }

    /// Whether the group at `index` is folded shut (see
    /// [`WorkspaceGroup::collapsed`]).
    pub fn is_collapsed(&self, index: usize) -> bool {
        self.groups
            .get(index)
            .is_some_and(WorkspaceGroup::collapsed)
    }

    /// Fold / unfold the group at `index`, returning its new collapsed state
    /// (`false` for an out-of-range index). The cursor and active row are kept on
    /// the same logical slot as the flat row space shrinks/grows: a cursor inside
    /// the folded group lands on its now-single header line.
    pub fn toggle_collapsed(&mut self, index: usize) -> bool {
        let Some(current) = self.groups.get(index).map(WorkspaceGroup::collapsed) else {
            return false;
        };
        self.set_group_collapsed(index, !current);
        !current
    }

    /// Apply the folded state recorded by workspace name after a list rebuild (a
    /// background re-sync builds a fresh list), so folds survive re-syncs. Names
    /// not present default to expanded. The cursor is left at the rebuild default
    /// (the first group's root), which is always valid.
    pub fn set_collapsed_by_names(&mut self, names: &std::collections::HashSet<String>) {
        for group in &mut self.groups {
            group.collapsed = names.contains(&group.name);
        }
    }

    /// Set the group at `index`'s folded state, re-resolving the cursor and active
    /// row through the row-space shift so neither is stranded on a hidden slot.
    /// Both indices are always in range, so their `(group, slot)` resolves; a slot
    /// in the now-folded group maps onto its single header line.
    fn set_group_collapsed(&mut self, index: usize, collapsed: bool) {
        let (sel_g, sel_slot) = self.locate(self.selected_index).unwrap();
        let (act_g, act_slot) = self.locate(self.active_index).unwrap();
        self.groups[index].collapsed = collapsed;
        self.selected_index = self.nav_index_of(sel_g, sel_slot);
        self.active_index = self.nav_index_of(act_g, act_slot);
    }

    /// The flat index of a `(group, slot)` (a valid group from [`locate`]). In a
    /// collapsed group only the root line exists, so every slot resolves to it. A
    /// worktree/create slot in an expanded group is clamped to the rows it has.
    fn nav_index_of(&self, group: usize, slot: RowSlot) -> usize {
        let start: usize = self
            .groups
            .iter()
            .take(group)
            .map(WorkspaceGroup::nav_slots)
            .sum();
        let g = &self.groups[group];
        let off = if g.collapsed {
            0
        } else {
            match slot {
                RowSlot::Root => 0,
                RowSlot::Worktree(i) => (i + 1).min(g.worktrees.len()),
                RowSlot::Create => g.worktrees.len() + 1,
            }
        };
        start + off
    }

    /// Replaces the PR links of the row whose session root is `root`, returning
    /// whether the set changed.
    ///
    /// Lets the attached pane or background watcher reflect a freshly detected
    /// pull-request URL in the sidebar `#N` badge immediately, instead of waiting
    /// for the next workspace re-sync (a slow, per-worktree `git status`) to fold
    /// `pr-links/` into `state.json`. The caller passes the store's accumulated,
    /// deduped set — the same value the re-sync would compute — so the live badge
    /// matches what a later sync produces. A `root` that matches no row (e.g. the
    /// workspace root, which has no worktree) is a no-op.
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
        matches!(self.locate(self.selected_index), Some((_, RowSlot::Root)))
    }

    /// Whether a root row is the active one.
    pub fn root_active(&self) -> bool {
        matches!(self.locate(self.active_index), Some((_, RowSlot::Root)))
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
            // Record the row being *left* by its `(root, name)` identity, so the
            // jump-back target is the exact session — not a same-named one in a
            // different 統合(unite) group.
            self.previous_active = Some((
                self.active_group_root().to_path_buf(),
                self.active_name().to_string(),
            ));
        }
        self.active_index = self.selected_index;
        self.active_name()
    }

    /// The previously active row's `(workspace root, session name)` identity, for
    /// carrying the `Ctrl-^` jump target across a list rebuild (a background
    /// re-sync drops the list and builds a fresh one). Paired with
    /// [`set_previous_active`](Self::set_previous_active).
    pub fn previous_active(&self) -> Option<&(PathBuf, String)> {
        self.previous_active.as_ref()
    }

    /// Restore the previously active row's identity after a rebuild, so the
    /// `Ctrl-^` jump survives a background re-sync. It is validated lazily by
    /// [`previous_row`](Self::previous_row), so one that no longer matches simply
    /// yields no jump rather than an error.
    pub fn set_previous_active(&mut self, previous: Option<(PathBuf, String)>) {
        self.previous_active = previous;
    }

    /// The flat row the previously active session now sits at (a group's root row
    /// for [`ROOT_NAME`]), or `None` when no previous session has been recorded
    /// yet or it has since been removed from the list. Resolved by `(root, name)`
    /// so a list rebuild keeps it pointing at the same session — the target
    /// `Ctrl-^` focuses — even when another group holds a same-named session.
    pub fn previous_row(&self) -> Option<usize> {
        let (root, name) = self.previous_active.as_ref()?;
        self.row_of_qualified(root, name)
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

    /// The flat row of the first *visible* worktree named `name` across all groups
    /// (a collapsed group's worktrees have no flat row, so they are skipped).
    fn row_of_name(&self, name: &str) -> Option<usize> {
        (0..self.nav_rows()).find(|&row| {
            self.worktree_at(row)
                .is_some_and(|w| worktree_name(w) == name)
        })
    }

    /// The flat row of the session named `name` in the group rooted at `root` — a
    /// group's root row for [`ROOT_NAME`], otherwise its matching worktree.
    /// Qualified by the group's root path so a same-named session in another
    /// 統合(unite) group is never matched. Backs the `Ctrl-^` jump-back
    /// ([`previous_row`](Self::previous_row)), whose target is carried by its stable
    /// `(root, name)` identity. `None` when no group has that root, or it has no
    /// such session.
    fn row_of_qualified(&self, root: &Path, name: &str) -> Option<usize> {
        let mut row = 0;
        for group in &self.groups {
            let here = group.root_path() == root;
            if here && name == ROOT_NAME {
                return Some(row);
            }
            // A folded group's worktrees have no flat row, so they cannot be jumped
            // to — skip the scan and let the row advance by the group's single slot.
            if !group.collapsed {
                for (i, w) in group.worktrees.iter().enumerate() {
                    if here && worktree_name(w) == name {
                        return Some(row + 1 + i);
                    }
                }
            }
            row += group.nav_slots();
        }
        None
    }

    /// The flat row of the `group`'s synthetic root row, or `None` when the group
    /// index is out of range. Each 統合(unite) group owns its own root row, so this
    /// restores the cursor onto a *specific* group's root across a rebuild — the
    /// plain [`ROOT_NAME`] lookup ([`row_of_name`](Self::row_of_name)) always
    /// resolves to the first group and cannot tell the extra groups' roots apart.
    pub fn group_root_row(&self, group: usize) -> Option<usize> {
        (group < self.groups.len()).then(|| {
            self.groups[..group]
                .iter()
                .map(WorkspaceGroup::nav_slots)
                .sum()
        })
    }

    /// The flat row of the first worktree named `name` **within** `group`, or
    /// `None` when that group has no such worktree. Restoring the cursor inside the
    /// same 統合(unite) group keeps it put when another workspace happens to carry a
    /// session with the same branch name (a plain [`row_of_name`](Self::row_of_name)
    /// would pull it into whichever group lists that name first).
    pub fn row_in_group_of_name(&self, group: usize, name: &str) -> Option<usize> {
        let start = self.group_root_row(group)?;
        // A folded group hides its worktrees, so none is navigable to restore onto.
        if self.groups[group].collapsed {
            return None;
        }
        self.groups[group]
            .worktrees
            .iter()
            .position(|w| worktree_name(w) == name)
            .map(|within| start + 1 + within)
    }

    /// The rows as command-facing [`WorktreeRef`]s (name + active flag): each
    /// group's root row, then its worktrees, in display order. Collapsed groups'
    /// worktrees are still listed (commands act on them by name) but can never be
    /// active, since the cursor cannot rest on a hidden row.
    pub fn refs(&self) -> Vec<WorktreeRef> {
        let mut refs = Vec::new();
        let mut nav = 0;
        for group in &self.groups {
            refs.push(WorktreeRef {
                name: ROOT_NAME.to_string(),
                active: nav == self.active_index,
            });
            nav += 1;
            if group.collapsed {
                for w in &group.worktrees {
                    refs.push(WorktreeRef {
                        name: worktree_name(w).to_string(),
                        active: false,
                    });
                }
                continue;
            }
            for w in &group.worktrees {
                refs.push(WorktreeRef {
                    name: worktree_name(w).to_string(),
                    active: nav == self.active_index,
                });
                nav += 1;
            }
            nav += 1; // the group's "+ new session" row
        }
        refs
    }

    /// Move the cursor directly to a flat selectable `row`, clamped to the rows
    /// that exist. Used by the session picker (`Ctrl-O`) to jump straight to the
    /// chosen session.
    pub fn focus_index(&mut self, row: usize) {
        self.selected_index = row.min(self.nav_rows().saturating_sub(1));
    }

    /// Move the *active* row directly to a flat selectable `row`, clamped to the
    /// rows that exist. Complements [`focus_index`](Self::focus_index) (which moves
    /// the cursor) so a rebuild can restore the active row by its resolved flat
    /// index without disturbing the `Ctrl-^` jump memory the name-based
    /// [`activate_by_name`](Self::activate_by_name) would.
    pub fn activate_index(&mut self, row: usize) {
        self.active_index = row.min(self.nav_rows().saturating_sub(1));
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
