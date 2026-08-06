//! Shell-owned live-terminal view controls.
//!
//! The controller reducer owns Home rows, overlays, and markers, but deliberately
//! *not* the live-terminal viewport's scrollback offset, in-progress selection, or
//! copy feedback: the migration design (`258-controller-runtime-migration.md`
//! §4.2) keeps terminal scroll / drag / copy a shell + [`TerminalSession`] concern
//! so they never round-trip through Home state. `LiveTerminalControls` holds that
//! per-frame state for the currently focused terminal.
//!
//! It is pure: the shell polls the [`TerminalSession`] for rows and cells and
//! drives the OS clipboard; this type only tracks scroll, an in-progress
//! [`TerminalSelection`], and the presentation-safe feedback line, and folds them
//! into the [`TerminalViewProjection`] the right pane renders. View state is
//! retained per terminal in a bounded cache, so temporarily focusing another
//! pane (for example the workspace Agent drawer) does not erase either view.
//!
//! [`TerminalSession`]: crate::usecase::application::terminal_session::TerminalSession

use std::collections::VecDeque;

use usagi_core::domain::id::TerminalRef;

use crate::presentation::views::workspace::TerminalViewProjection;
use crate::usecase::application::pr::BrowserOpener;
use crate::usecase::application::terminal_link::{url_at, validate_url};
use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};

const RETAINED_TERMINAL_VIEW_LIMIT: usize = 8;

/// Scroll, selection, and feedback for one live terminal.
#[derive(Debug, Default)]
struct TerminalViewState {
    /// Rows scrolled up from the live bottom.
    scroll: usize,
    /// The furthest the current viewport can scroll, recomputed each frame from
    /// the retained rows so `scroll_up` cannot run past the top.
    max_scroll: usize,
    /// A left-button press that has not moved yet. Keeping this separate from
    /// `selection` is what distinguishes a click from a one-cell drag: the
    /// selection becomes visible and copyable only after the first drag event.
    pointer_press: Option<TerminalSelection>,
    /// The drag selection, snapshotted at its anchor. It is retained after the
    /// mouse is released so the highlighted range stays on screen (and copyable)
    /// until a new drag replaces it or the terminal is closed/evicted.
    selection: Option<TerminalSelection>,
    /// Whether a mouse drag is currently extending [`Self::selection`]. This
    /// distinguishes "extend the live drag" from "start a fresh selection",
    /// which `has_selection` alone cannot once a finished selection lingers.
    dragging: bool,
    /// The presentation-safe feedback line shown in the right-pane footer.
    feedback: Option<String>,
}

/// Terminal-local view controls owned by the runtime shell rather than the
/// controller reducer.
///
/// `active` is the focused terminal's state. States left by a focus transition
/// are retained oldest-first in `retained`; together they are bounded by
/// [`RETAINED_TERMINAL_VIEW_LIMIT`].
#[derive(Debug, Default)]
pub struct LiveTerminalControls {
    focused: Option<TerminalRef>,
    active: TerminalViewState,
    retained: VecDeque<(TerminalRef, TerminalViewState)>,
    revision: u64,
}

impl LiveTerminalControls {
    /// Track `terminal` as the focused pane, restoring its terminal-local view
    /// state when focus returns. A pending pointer gesture cannot span panes, so
    /// blur cancels the gesture while retaining an established selection.
    pub fn sync_focus(&mut self, terminal: Option<&TerminalRef>) {
        if self.focused.as_ref() == terminal {
            return;
        }
        self.revision = self.revision.saturating_add(1);
        if let Some(previous) = self.focused.take() {
            self.active.pointer_press = None;
            self.active.dragging = false;
            self.retained
                .retain(|(retained, _)| !retained.fences(&previous));
            self.retained
                .push_back((previous, std::mem::take(&mut self.active)));
        } else {
            self.active = TerminalViewState::default();
        }

        self.focused = terminal.cloned();
        if let Some(terminal) = terminal {
            self.active = self
                .retained
                .iter()
                .position(|(retained, _)| retained.fences(terminal))
                .and_then(|position| self.retained.remove(position))
                .map_or_else(TerminalViewState::default, |(_, state)| state);
            self.trim_retained(RETAINED_TERMINAL_VIEW_LIMIT - 1);
        } else {
            self.trim_retained(RETAINED_TERMINAL_VIEW_LIMIT);
        }
    }

    /// Forget view state for terminals no longer represented by a live tab.
    /// Foreground detach does not call this; logical close and exit do.
    pub fn retain_terminals(&mut self, terminals: &[TerminalRef]) {
        let retained_len = self.retained.len();
        let focused = self.focused.clone();
        let is_live =
            |candidate: &TerminalRef| terminals.iter().any(|terminal| terminal.fences(candidate));
        self.retained.retain(|(terminal, _)| is_live(terminal));
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| !is_live(focused))
        {
            self.focused = None;
            self.active = TerminalViewState::default();
        }
        if retained_len != self.retained.len() || focused != self.focused {
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn trim_retained(&mut self, limit: usize) {
        while self.retained.len() > limit {
            self.retained.pop_front();
        }
    }

    /// Scroll one line toward older output, clamped to the last projected extent.
    pub fn scroll_up(&mut self) {
        let before = self.active.scroll;
        self.active.scroll = self
            .active
            .scroll
            .saturating_add(1)
            .min(self.active.max_scroll);
        self.revision = self
            .revision
            .saturating_add(u64::from(before != self.active.scroll));
    }

    /// Scroll one line back toward the live bottom.
    pub fn scroll_down(&mut self) {
        let before = self.active.scroll;
        self.active.scroll = self.active.scroll.saturating_sub(1);
        self.revision = self
            .revision
            .saturating_add(u64::from(before != self.active.scroll));
    }

    /// Begin a drag selection, replacing any earlier (including finished) one,
    /// and surface that a selection has started.
    pub fn begin_selection(&mut self, selection: TerminalSelection) {
        self.revision = self.revision.saturating_add(1);
        self.active.pointer_press = None;
        self.active.selection = Some(selection);
        self.active.dragging = true;
        self.active.feedback = Some("terminal selection started".to_owned());
    }

    /// Record a pointer press without starting a text selection, clearing any
    /// retained selection immediately. A subsequent drag promotes the
    /// snapshotted viewport and anchor into `selection`; a release before that
    /// remains a plain click.
    pub fn press_pointer(&mut self, selection: TerminalSelection) {
        self.revision = self.revision.saturating_add(1);
        self.active.pointer_press = Some(selection);
        self.active.selection = None;
        self.active.dragging = false;
    }

    /// Promote a pending press into a drag selection and extend it to `focus`.
    /// Returns `false` for a stray drag that had no preceding press.
    pub fn drag_pointer(&mut self, focus: TerminalPoint) -> bool {
        if !self.active.dragging {
            let Some(selection) = self.active.pointer_press.take() else {
                return false;
            };
            self.active.selection = Some(selection);
            self.active.dragging = true;
            self.active.feedback = Some("terminal selection started".to_owned());
        }
        self.extend_selection(focus);
        self.revision = self.revision.saturating_add(1);
        true
    }

    /// Complete the current pointer gesture.
    ///
    /// A press released before any drag is a click. A promoted drag returns its
    /// selected text for copying. A release without a matching press is inert.
    pub fn release_pointer(&mut self) -> PointerRelease {
        self.revision = self.revision.saturating_add(1);
        if self.active.dragging {
            return self
                .finish_drag()
                .map_or(PointerRelease::None, PointerRelease::Copy);
        }
        if self.active.pointer_press.take().is_some() {
            return PointerRelease::Click;
        }
        PointerRelease::None
    }

    /// Extend the in-progress selection to `focus`; a no-op without a selection.
    pub fn extend_selection(&mut self, focus: TerminalPoint) {
        if let Some(selection) = &mut self.active.selection {
            selection.extend(focus);
            self.revision = self.revision.saturating_add(1);
        }
    }

    /// Whether a selection currently exists (either an active drag or a finished
    /// one still highlighted on screen).
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.active.selection.is_some()
    }

    /// Whether a mouse drag is actively extending the selection. The shell uses
    /// this to decide whether a drag event extends the live selection or starts
    /// a fresh one over a lingering, finished selection.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.active.dragging
    }

    /// The current selection, so the shell renders the highlighted rows. Kept
    /// after the mouse is released so the range stays visible.
    #[must_use]
    pub fn selection(&self) -> Option<&TerminalSelection> {
        self.active.selection.as_ref()
    }

    /// End an in-progress drag and return the finished selection's text to copy,
    /// keeping the selection highlighted on screen. Returns `None` when no drag
    /// was active (so a stray release never re-copies or clears the clipboard) or
    /// when the selection is empty — an empty selection is dropped with safe
    /// feedback instead of lingering as an invisible highlight.
    pub fn finish_drag(&mut self) -> Option<String> {
        if !self.active.dragging {
            return None;
        }
        self.revision = self.revision.saturating_add(1);
        self.active.dragging = false;
        let text = self.active.selection.as_ref()?.text();
        if text.is_empty() {
            self.active.selection = None;
            self.active.feedback = Some("no terminal text is selected".to_owned());
            None
        } else {
            Some(text)
        }
    }

    /// Explicitly drop a retained selection while preserving scroll position
    /// and the focused terminal.
    pub fn clear_selection(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.active.pointer_press = None;
        self.active.selection = None;
        self.active.dragging = false;
        self.active.feedback = Some("terminal selection cleared".to_owned());
    }

    /// Record the outcome of writing `text` to the OS clipboard as feedback.
    pub fn record_copy(&mut self, text: &str, result: Result<(), String>) {
        self.revision = self.revision.saturating_add(1);
        self.active.feedback = Some(match result {
            Ok(()) => {
                let lines = text.lines().count().max(1);
                let suffix = if lines == 1 { "" } else { "s" };
                format!("copied {lines} line{suffix}")
            }
            Err(message) => message,
        });
    }

    /// Monotonic cache fence for scroll, selection, focus and feedback.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Open the `http(s)` URL under a plain terminal click through the injected
    /// browser, recording presentation-safe feedback on success or failure.
    ///
    /// The click cell is hit-tested against the snapshotted `cells` with the pure
    /// #387 detector ([`url_at`]) and re-validated ([`validate_url`]) immediately
    /// before spawning, so an ANSI/control sequence can never reach a browser
    /// argument. A click that lands off any link is a silent no-op (`false`): it
    /// opens nothing and leaves feedback untouched, so it does not disturb the
    /// shell or the child PTY. `browser` is the argv-based platform adapter, which
    /// never invokes a shell.
    ///
    /// The caller only reaches this after [`finish_drag`](Self::finish_drag)
    /// yields nothing, so a non-empty drag selection copies and a plain click
    /// opens — the two gestures never both fire.
    pub fn open_link_at(
        &mut self,
        cells: &[String],
        point: TerminalPoint,
        browser: &mut dyn BrowserOpener,
    ) -> bool {
        let Some(url) = url_at(cells, point)
            .and_then(|candidate| validate_url(&candidate).ok().map(str::to_owned))
        else {
            return false;
        };
        self.revision = self.revision.saturating_add(1);
        self.active.feedback = Some(match browser.open(&url) {
            Ok(()) => format!("opened {url}"),
            Err(message) => format!("Could not open browser: {message}"),
        });
        true
    }

    /// Replace the feedback line with a presentation-safe message.
    pub fn set_feedback(&mut self, message: impl Into<String>) {
        self.active.feedback = Some(message.into());
        self.revision = self.revision.saturating_add(1);
    }

    /// Project `rows` into the right-pane viewport at the current scroll offset,
    /// recomputing the scroll extent from the row count and `viewport_rows` so a
    /// shrunk history re-clamps the offset.
    pub fn project(&mut self, rows: Vec<String>, viewport_rows: usize) -> TerminalViewProjection {
        let total_rows = rows.len();
        self.active.max_scroll = total_rows.saturating_sub(viewport_rows);
        self.active.scroll = self.active.scroll.min(self.active.max_scroll);
        TerminalViewProjection {
            rows,
            row_offset: 0,
            total_rows,
            scroll: self.active.scroll,
            feedback: self.active.feedback.clone(),
        }
    }

    /// Clamp the current scroll offset against a retained row count and return
    /// the exact row range needed for the visible viewport.
    #[must_use]
    pub fn visible_range(
        &mut self,
        total_rows: usize,
        viewport_rows: usize,
    ) -> std::ops::Range<usize> {
        self.active.max_scroll = total_rows.saturating_sub(viewport_rows);
        self.active.scroll = self.active.scroll.min(self.active.max_scroll);
        let end = total_rows.saturating_sub(self.active.scroll);
        end.saturating_sub(viewport_rows)..end
    }

    /// Project an already-windowed row range while retaining global scroll
    /// coordinates for rendering and pointer hit testing.
    #[must_use]
    pub fn project_window(
        &self,
        rows: Vec<String>,
        row_offset: usize,
        total_rows: usize,
    ) -> TerminalViewProjection {
        TerminalViewProjection {
            rows,
            row_offset,
            total_rows,
            scroll: self.active.scroll,
            feedback: self.active.feedback.clone(),
        }
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.active.scroll
    }
}

/// Result of completing one terminal pointer gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerRelease {
    /// No matching pointer press was active.
    None,
    /// The pointer was released without a drag.
    Click,
    /// A drag selection completed with non-empty text.
    Copy(String),
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{LiveTerminalControls, PointerRelease};
    use crate::usecase::application::pr::BrowserOpener;
    use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn rows(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("row {index}")).collect()
    }

    #[test]
    fn scroll_is_clamped_to_the_projected_extent() {
        let mut controls = LiveTerminalControls::default();
        // Ten rows into a five-row viewport can scroll five lines up.
        let _ = controls.project(rows(10), 5);
        for _ in 0..8 {
            controls.scroll_up();
        }
        assert_eq!(controls.project(rows(10), 5).scroll, 5);
        controls.scroll_down();
        assert_eq!(controls.project(rows(10), 5).scroll, 4);
    }

    #[test]
    fn a_shrunk_history_re_clamps_the_stored_offset() {
        let mut controls = LiveTerminalControls::default();
        let _ = controls.project(rows(20), 5);
        for _ in 0..10 {
            controls.scroll_up();
        }
        assert_eq!(controls.project(rows(20), 5).scroll, 10);
        // The history collapsed to fit the viewport; the offset clamps to zero.
        assert_eq!(controls.project(rows(4), 5).scroll, 0);
    }

    #[test]
    fn changing_focus_restores_each_terminals_scroll_selection_and_feedback() {
        let mut controls = LiveTerminalControls::default();
        let first = terminal();
        let second = terminal();
        controls.sync_focus(Some(&first));
        let _ = controls.project(rows(10), 3);
        controls.scroll_up();
        controls.begin_selection(TerminalSelection::begin(
            vec!["hi".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 1 });
        let _ = controls.finish_drag();
        controls.record_copy("hi", Ok(()));
        assert!(controls.has_selection());

        // Re-syncing the same terminal keeps the state.
        controls.sync_focus(Some(&first));
        assert!(controls.has_selection());

        // A second terminal starts clean and keeps an independent view.
        controls.sync_focus(Some(&second));
        let _ = controls.project(rows(10), 3);
        controls.scroll_up();
        controls.scroll_up();
        controls.begin_selection(TerminalSelection::begin(
            vec!["second".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 5 });
        let _ = controls.finish_drag();

        controls.sync_focus(Some(&first));
        assert_eq!(controls.project(rows(10), 3).scroll, 1);
        assert_eq!(
            controls.selection().map(TerminalSelection::text).as_deref(),
            Some("hi")
        );
        assert_eq!(
            controls.project(rows(10), 3).feedback.as_deref(),
            Some("copied 1 line")
        );
        controls.sync_focus(None);
        controls.sync_focus(Some(&second));
        assert_eq!(controls.project(rows(10), 3).scroll, 2);
        assert_eq!(
            controls.selection().map(TerminalSelection::text).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn retained_terminal_views_are_bounded_and_closed_terminals_are_forgotten() {
        let terminals = (0..=super::RETAINED_TERMINAL_VIEW_LIMIT)
            .map(|_| terminal())
            .collect::<Vec<_>>();
        let mut controls = LiveTerminalControls::default();

        for (index, terminal) in terminals.iter().enumerate() {
            controls.sync_focus(Some(terminal));
            controls.set_feedback(format!("terminal {index}"));
        }

        // The oldest of nine identities was evicted from the eight-entry cache.
        controls.sync_focus(Some(&terminals[0]));
        assert_eq!(controls.project(rows(1), 1).feedback, None);

        // A logically closed terminal is forgotten even if it was focused.
        controls.set_feedback("must be dropped");
        controls.retain_terminals(&terminals[1..]);
        controls.sync_focus(Some(&terminals[0]));
        assert_eq!(controls.project(rows(1), 1).feedback, None);
    }

    #[test]
    fn begin_and_extend_build_the_copy_text_and_keep_the_selection_after_release() {
        let mut controls = LiveTerminalControls::default();
        controls.begin_selection(TerminalSelection::begin(
            vec!["hello".to_owned(), "world".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        assert!(controls.is_dragging());
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("terminal selection started")
        );
        controls.extend_selection(TerminalPoint { row: 1, column: 4 });
        assert!(controls.selection().is_some());
        let text = controls.finish_drag().expect("non-empty selection");
        assert_eq!(text, "hello\nworld");
        // Releasing copies but keeps the range highlighted; the drag is over.
        assert!(controls.has_selection());
        assert!(!controls.is_dragging());
        // A stray release without a live drag must not re-copy the retained text.
        assert!(controls.finish_drag().is_none());
        assert!(controls.has_selection());
    }

    #[test]
    fn pointer_gesture_distinguishes_click_from_drag_before_selecting() {
        let viewport = vec!["hello".to_owned()];
        let anchor = TerminalPoint { row: 0, column: 0 };
        let mut controls = LiveTerminalControls::default();

        controls.press_pointer(TerminalSelection::begin(viewport.clone(), anchor));
        assert!(!controls.has_selection());
        assert!(!controls.is_dragging());
        assert_eq!(controls.release_pointer(), PointerRelease::Click);

        controls.press_pointer(TerminalSelection::begin(viewport, anchor));
        assert!(controls.drag_pointer(TerminalPoint { row: 0, column: 2 }));
        assert!(controls.drag_pointer(TerminalPoint { row: 0, column: 4 }));
        assert!(controls.has_selection());
        assert!(controls.is_dragging());
        assert_eq!(
            controls.release_pointer(),
            PointerRelease::Copy("hello".to_owned())
        );
        assert!(controls.has_selection());
        assert!(!controls.is_dragging());
        assert_eq!(controls.release_pointer(), PointerRelease::None);
    }

    #[test]
    fn a_plain_pointer_press_clears_a_finished_selection() {
        let viewport = vec!["hello".to_owned()];
        let anchor = TerminalPoint { row: 0, column: 0 };
        let mut controls = LiveTerminalControls::default();
        controls.begin_selection(TerminalSelection::begin(viewport.clone(), anchor));
        controls.extend_selection(TerminalPoint { row: 0, column: 4 });
        assert_eq!(controls.finish_drag().as_deref(), Some("hello"));
        assert!(controls.has_selection());

        controls.press_pointer(TerminalSelection::begin(viewport, anchor));

        assert!(!controls.has_selection());
        assert_eq!(controls.release_pointer(), PointerRelease::Click);
    }

    #[test]
    fn a_new_drag_replaces_a_finished_selection() {
        let mut controls = LiveTerminalControls::default();
        controls.begin_selection(TerminalSelection::begin(
            vec!["first".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 4 });
        assert_eq!(controls.finish_drag().as_deref(), Some("first"));
        // A finished selection lingers; the next drag begins a fresh one instead
        // of extending it.
        assert!(controls.has_selection() && !controls.is_dragging());
        controls.begin_selection(TerminalSelection::begin(
            vec!["second".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 2 });
        assert_eq!(controls.finish_drag().as_deref(), Some("sec"));
    }

    #[test]
    fn extend_without_a_selection_is_inert() {
        let mut controls = LiveTerminalControls::default();
        controls.extend_selection(TerminalPoint { row: 0, column: 0 });
        assert!(controls.finish_drag().is_none());
    }

    #[test]
    fn clearing_a_retained_selection_preserves_scroll_and_focus() {
        let mut controls = LiveTerminalControls::default();
        let terminal = terminal();
        controls.sync_focus(Some(&terminal));
        let _ = controls.project(rows(10), 5);
        controls.scroll_up();
        controls.begin_selection(TerminalSelection::begin(
            vec!["hello".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        let _ = controls.finish_drag();

        controls.clear_selection();

        assert!(!controls.has_selection());
        assert!(!controls.is_dragging());
        assert_eq!(controls.project(rows(10), 5).scroll, 1);
        assert_eq!(
            controls.project(rows(10), 5).feedback.as_deref(),
            Some("terminal selection cleared")
        );
    }

    #[test]
    fn an_empty_selection_is_dropped_with_feedback_without_clearing_the_clipboard() {
        let mut controls = LiveTerminalControls::default();
        controls.begin_selection(TerminalSelection::begin(
            vec!["text".to_owned()],
            TerminalPoint { row: 0, column: 9 },
        ));
        assert!(controls.finish_drag().is_none());
        // An empty selection is not left lingering as an invisible highlight.
        assert!(!controls.has_selection());
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("no terminal text is selected")
        );
    }

    #[test]
    fn record_copy_reports_line_counts_and_clipboard_errors() {
        let mut controls = LiveTerminalControls::default();
        controls.record_copy("only", Ok(()));
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("copied 1 line")
        );
        controls.record_copy("a\nb\nc", Ok(()));
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("copied 3 lines")
        );
        controls.record_copy("x", Err("clipboard is unavailable".to_owned()));
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("clipboard is unavailable")
        );
    }

    #[test]
    fn set_feedback_surfaces_a_safe_message() {
        let mut controls = LiveTerminalControls::default();
        controls.set_feedback("terminal is busy");
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("terminal is busy")
        );
    }

    #[derive(Default)]
    struct FakeBrowser {
        opened: Vec<String>,
        error: Option<String>,
    }

    impl BrowserOpener for FakeBrowser {
        fn open(&mut self, url: &str) -> Result<(), String> {
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            self.opened.push(url.to_owned());
            Ok(())
        }
    }

    fn viewport() -> Vec<String> {
        vec!["see https://example.com/x now".to_owned()]
    }

    #[test]
    fn a_click_on_a_link_opens_it_and_reports_it() {
        let mut controls = LiveTerminalControls::default();
        let mut browser = FakeBrowser::default();
        // Cols 4..=24 sit on the URL; a click anywhere along it opens the whole
        // link and reports it once.
        assert!(controls.open_link_at(
            &viewport(),
            TerminalPoint { row: 0, column: 10 },
            &mut browser
        ));
        assert_eq!(browser.opened, vec!["https://example.com/x".to_owned()]);
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("opened https://example.com/x")
        );
    }

    #[test]
    fn the_same_link_opens_every_time_it_is_clicked() {
        let mut controls = LiveTerminalControls::default();
        let mut browser = FakeBrowser::default();
        let point = TerminalPoint { row: 0, column: 4 };
        assert!(controls.open_link_at(&viewport(), point, &mut browser));
        assert!(controls.open_link_at(&viewport(), point, &mut browser));
        // Detection reads the grid each time, so no state is consumed: a repeat
        // click opens the link again.
        assert_eq!(
            browser.opened,
            vec![
                "https://example.com/x".to_owned(),
                "https://example.com/x".to_owned()
            ]
        );
    }

    #[test]
    fn a_click_off_any_link_opens_nothing_and_leaves_feedback_untouched() {
        let mut controls = LiveTerminalControls::default();
        controls.set_feedback("earlier");
        let mut browser = FakeBrowser::default();
        // The leading "see" word and the trailing blank padding are not links.
        assert!(!controls.open_link_at(
            &viewport(),
            TerminalPoint { row: 0, column: 0 },
            &mut browser
        ));
        assert!(!controls.open_link_at(
            &viewport(),
            TerminalPoint { row: 0, column: 28 },
            &mut browser
        ));
        assert!(browser.opened.is_empty());
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("earlier")
        );
    }

    #[test]
    fn a_browser_launch_failure_reports_a_safe_notice() {
        let mut controls = LiveTerminalControls::default();
        let mut browser = FakeBrowser {
            error: Some("browser launch failed".to_owned()),
            ..FakeBrowser::default()
        };
        assert!(controls.open_link_at(
            &viewport(),
            TerminalPoint { row: 0, column: 5 },
            &mut browser
        ));
        assert!(browser.opened.is_empty());
        assert_eq!(
            controls.project(rows(1), 1).feedback.as_deref(),
            Some("Could not open browser: browser launch failed")
        );
    }
}
