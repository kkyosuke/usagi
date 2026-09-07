//! Pure finder/document state and reducer for the Home Preview overlay.

use unicode_segmentation::UnicodeSegmentation;
use usagi_core::domain::id::RequestId;
use usagi_core::domain::presentation_text::presentation_character_is_safe;

use crate::usecase::fuzzy::fuzzy_score;

use super::{AppKey, AppState, Effect, SafeError, Target};

const MAX_PREVIEW_FILTER_CHARS: usize = 256;
const MAX_PREVIEW_SEARCH_CHARS: usize = 256;
const MAX_PREVIEW_SEARCH_MATCHES: usize = 20_000;

/// Repository file group shown by the Preview finder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewFileFilter {
    /// Tracked files plus untracked files that are not ignored.
    #[default]
    All,
    /// Tracked files changed from the integration base, plus untracked non-ignored files.
    Changed,
    /// Files tracked by git, whether changed or clean.
    Tracked,
}

impl PreviewFileFilter {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::All => Self::Changed,
            Self::Changed => Self::Tracked,
            Self::Tracked => Self::All,
        }
    }

    pub(super) const fn previous(self) -> Self {
        match self {
            Self::All => Self::Tracked,
            Self::Changed => Self::All,
            Self::Tracked => Self::Changed,
        }
    }

    /// Finder tabs in their horizontal navigation order.
    pub const TABS: [Self; 3] = [Self::All, Self::Changed, Self::Tracked];

    #[must_use]
    pub const fn tab_index(self) -> usize {
        match self {
            Self::All => 0,
            Self::Changed => 1,
            Self::Tracked => 2,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Changed => "changed",
            Self::Tracked => "tracked",
        }
    }
}

/// One literal document-search occurrence in the loaded Preview file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewSearchMatch {
    line: usize,
    start: usize,
    end: usize,
}

impl PreviewSearchMatch {
    #[must_use]
    pub const fn line(self) -> usize {
        self.line
    }

    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PreviewDisplay {
    line_numbers: bool,
    wrap_lines: bool,
}

/// Selected-session file finder and read-only text preview state.
///
/// Repository-relative paths and file lines return through [`Effect::LoadPreview`].
/// The reducer owns filtering, selection, the finder/document transition, and
/// document scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewOverlay {
    pub(super) target: Target,
    pub(super) request_id: RequestId,
    pub(super) files: Vec<String>,
    pub(super) filter: String,
    pub(super) file_filter: PreviewFileFilter,
    pub(super) selected: usize,
    pub(super) path: Option<String>,
    pub(super) lines: Vec<String>,
    /// Absolute projected-row scroll when no search match owns the anchor.
    pub(super) scroll: usize,
    /// Signed projected-row delta from the active search match.
    match_scroll: Option<isize>,
    pub(super) search: String,
    pub(super) search_editing: bool,
    pub(super) current_match: usize,
    display: PreviewDisplay,
    pub(super) loading: bool,
    pub(super) error: Option<SafeError>,
}

impl PreviewOverlay {
    pub(super) fn loading(target: Target) -> Self {
        Self {
            target,
            request_id: RequestId::new(),
            files: Vec::new(),
            filter: String::new(),
            file_filter: PreviewFileFilter::All,
            selected: 0,
            path: None,
            lines: Vec::new(),
            scroll: 0,
            match_scroll: None,
            search: String::new(),
            search_editing: false,
            current_match: 0,
            display: PreviewDisplay::default(),
            loading: true,
            error: None,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// Identity of the latest finder/document load accepted by this overlay.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }
    /// Current fuzzy filter.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }
    /// Repository file group currently shown by the finder.
    #[must_use]
    pub const fn file_filter(&self) -> PreviewFileFilter {
        self.file_filter
    }
    /// Filtered file paths in fuzzy rank order.
    #[must_use]
    pub fn visible_files(&self) -> Vec<&str> {
        let mut files = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(order, path)| {
                fuzzy_score(path, &self.filter).map(|score| (score, order, path.as_str()))
            })
            .collect::<Vec<_>>();
        if !self.filter.is_empty() {
            files.sort_by_key(|(score, order, _)| (*score, *order));
        }
        files.into_iter().map(|(_, _, path)| path).collect()
    }
    /// Selected row within [`Self::visible_files`].
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }
    /// Selected repository-relative path, if the current filter has a match.
    #[must_use]
    pub fn selected_file(&self) -> Option<&str> {
        self.visible_files().get(self.selected).copied()
    }
    /// Open repository-relative document path. `None` means the finder is open.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
    /// 表示可能な preview 行。素材未着なら空。
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
    /// Current absolute projected-row offset when search is not anchoring the view.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }
    /// Resolve the current scroll after presentation has projected wrapped rows.
    #[must_use]
    pub fn projected_scroll(&self, active_match_row: Option<usize>, row_count: usize) -> usize {
        let scroll = match (self.match_scroll, active_match_row) {
            (Some(delta), Some(anchor)) if delta.is_negative() => {
                anchor.saturating_sub(delta.unsigned_abs())
            }
            (Some(delta), Some(anchor)) => anchor.saturating_add(delta.unsigned_abs()),
            _ => self.scroll,
        };
        scroll.min(row_count.saturating_sub(1))
    }
    /// Literal, case-sensitive query retained by the document viewer.
    #[must_use]
    pub fn search(&self) -> &str {
        &self.search
    }
    /// Whether ordinary text input currently belongs to the document search.
    #[must_use]
    pub const fn is_search_editing(&self) -> bool {
        self.search_editing
    }
    /// Every non-overlapping occurrence in source order.
    #[must_use]
    pub fn search_matches(&self) -> Vec<PreviewSearchMatch> {
        if self.search.is_empty() {
            return Vec::new();
        }
        self.lines
            .iter()
            .enumerate()
            .flat_map(|(line, text)| {
                text.match_indices(&self.search)
                    .map(move |(start, matched)| PreviewSearchMatch {
                        line,
                        start,
                        end: start + matched.len(),
                    })
            })
            .take(MAX_PREVIEW_SEARCH_MATCHES)
            .collect()
    }
    /// Zero-based index of the active occurrence within [`Self::search_matches`].
    #[must_use]
    pub const fn current_match(&self) -> usize {
        self.current_match
    }
    /// Whether source line numbers are shown in the document viewer.
    #[must_use]
    pub const fn show_line_numbers(&self) -> bool {
        self.display.line_numbers
    }
    /// Whether long source lines wrap instead of being clipped.
    #[must_use]
    pub const fn wrap_lines(&self) -> bool {
        self.display.wrap_lines
    }
    /// Whether the finder or selected document is waiting for backend data.
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        self.loading
    }
    /// port が分類した安全なエラー。
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }

    fn begin_request(&mut self) -> RequestId {
        self.request_id = RequestId::new();
        self.request_id
    }

    fn reset_search_position(&mut self) {
        self.current_match = 0;
        self.match_scroll = (!self.search_matches().is_empty()).then_some(0);
    }

    fn move_search_match(&mut self, forward: bool) {
        let matches = self.search_matches();
        if matches.is_empty() {
            self.current_match = 0;
            self.match_scroll = None;
            return;
        }
        self.current_match = if forward {
            (self.current_match + 1) % matches.len()
        } else {
            (self.current_match + matches.len() - 1) % matches.len()
        };
        self.match_scroll = Some(0);
    }

    fn scroll_up(&mut self) {
        if let Some(delta) = self.match_scroll.as_mut() {
            *delta = delta.saturating_sub(1);
        } else {
            self.scroll = self.scroll.saturating_sub(1);
        }
    }

    fn scroll_down(&mut self) {
        if let Some(delta) = self.match_scroll.as_mut() {
            *delta = delta.saturating_add(1);
        } else {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    fn reset_projection_scroll(&mut self) {
        if let Some(delta) = self.match_scroll.as_mut() {
            *delta = 0;
        } else {
            self.scroll = 0;
        }
    }
}

/// Preview overlay input. The finder accepts a fuzzy filter, switches repository
/// file groups, and opens its selected file. The document searches, changes its
/// display projection, scrolls, and returns to the cached finder on Esc.
pub(super) fn update_preview_overlay(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(document_open) = state
        .preview_overlay
        .as_ref()
        .map(|overlay| overlay.path.is_some())
    else {
        state.overlay = None;
        return vec![Effect::CancelPreview];
    };

    if document_open {
        return update_preview_document(state.preview_overlay.as_mut().unwrap(), key);
    }
    update_preview_finder(state, key)
}

fn update_preview_document(overlay: &mut PreviewOverlay, key: &AppKey) -> Vec<Effect> {
    if overlay.search_editing {
        update_preview_search(overlay, key);
        return Vec::new();
    }
    match key {
        AppKey::Escape => {
            overlay.path = None;
            overlay.lines.clear();
            overlay.scroll = 0;
            overlay.match_scroll = None;
            overlay.search.clear();
            overlay.current_match = 0;
            overlay.loading = false;
            overlay.error = None;
            return vec![Effect::CancelPreview];
        }
        AppKey::Up => overlay.scroll_up(),
        AppKey::Down => overlay.scroll_down(),
        AppKey::Char('/') => {
            overlay.search.clear();
            overlay.current_match = 0;
            overlay.match_scroll = None;
            overlay.search_editing = true;
        }
        AppKey::Char('n') => overlay.move_search_match(true),
        AppKey::Char('N') => overlay.move_search_match(false),
        AppKey::Char('l') => {
            overlay.display.line_numbers = !overlay.display.line_numbers;
            overlay.reset_projection_scroll();
        }
        AppKey::Char('w') => {
            overlay.display.wrap_lines = !overlay.display.wrap_lines;
            overlay.reset_projection_scroll();
        }
        _ => {}
    }
    Vec::new()
}

fn update_preview_search(overlay: &mut PreviewOverlay, key: &AppKey) {
    match key {
        AppKey::Escape | AppKey::Enter => overlay.search_editing = false,
        AppKey::Backspace => {
            pop_last_grapheme(&mut overlay.search);
            overlay.reset_search_position();
        }
        AppKey::Char(character) if presentation_character_is_safe(*character) => {
            if overlay.search.chars().count() < MAX_PREVIEW_SEARCH_CHARS {
                overlay.search.push(*character);
                overlay.reset_search_position();
            }
        }
        AppKey::Paste(text) => {
            let remaining = MAX_PREVIEW_SEARCH_CHARS.saturating_sub(overlay.search.chars().count());
            overlay.search.extend(
                text.chars()
                    .filter(|character| presentation_character_is_safe(*character))
                    .take(remaining),
            );
            overlay.reset_search_position();
        }
        _ => {}
    }
}

fn update_preview_finder(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.preview_overlay = None;
            return vec![Effect::CancelPreview];
        }
        AppKey::Left | AppKey::Right => {
            let overlay = state.preview_overlay.as_mut().unwrap();
            overlay.file_filter = if matches!(key, AppKey::Right) {
                overlay.file_filter.next()
            } else {
                overlay.file_filter.previous()
            };
            overlay.files.clear();
            overlay.selected = 0;
            overlay.loading = true;
            overlay.error = None;
            let request_id = overlay.begin_request();
            return vec![Effect::LoadPreview {
                target: overlay.target,
                request_id,
                path: None,
                filter: overlay.file_filter,
            }];
        }
        AppKey::Up => {
            let overlay = state.preview_overlay.as_mut().unwrap();
            overlay.selected = overlay.selected.saturating_sub(1);
        }
        AppKey::Down => {
            let visible_len = state
                .preview_overlay
                .as_ref()
                .unwrap()
                .visible_files()
                .len();
            let overlay = state.preview_overlay.as_mut().unwrap();
            overlay.selected = (overlay.selected + 1).min(visible_len.saturating_sub(1));
        }
        AppKey::Backspace => {
            let overlay = state.preview_overlay.as_mut().unwrap();
            pop_last_grapheme(&mut overlay.filter);
            overlay.selected = 0;
        }
        AppKey::Char(character) if presentation_character_is_safe(*character) => {
            let overlay = state.preview_overlay.as_mut().unwrap();
            if overlay.filter.chars().count() < MAX_PREVIEW_FILTER_CHARS {
                overlay.filter.push(*character);
                overlay.selected = 0;
            }
        }
        AppKey::Paste(text) => {
            let overlay = state.preview_overlay.as_mut().unwrap();
            let remaining = MAX_PREVIEW_FILTER_CHARS.saturating_sub(overlay.filter.chars().count());
            overlay.filter.extend(
                text.chars()
                    .filter(|character| presentation_character_is_safe(*character))
                    .take(remaining),
            );
            overlay.selected = 0;
        }
        AppKey::Enter => {
            let Some((target, path)) = state.preview_overlay.as_ref().and_then(|overlay| {
                overlay
                    .selected_file()
                    .map(|path| (overlay.target, path.to_owned()))
            }) else {
                return Vec::new();
            };
            let overlay = state.preview_overlay.as_mut().unwrap();
            overlay.path = Some(path.clone());
            overlay.lines.clear();
            overlay.scroll = 0;
            overlay.match_scroll = None;
            overlay.loading = true;
            overlay.error = None;
            let request_id = overlay.begin_request();
            return vec![Effect::LoadPreview {
                target,
                request_id,
                path: Some(path),
                filter: overlay.file_filter,
            }];
        }
        _ => {}
    }
    Vec::new()
}

pub(super) fn sanitize_preview_line(line: &str) -> String {
    line.chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if presentation_character_is_safe(character) {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

fn pop_last_grapheme(value: &mut String) {
    if let Some((start, _)) = value.grapheme_indices(true).next_back() {
        value.truncate(start);
    }
}

#[cfg(test)]
mod tests {
    use usagi_core::domain::id::{SessionId, WorkspaceId};

    use super::super::{AppEvent, BackendEvent, Overlay, update};
    use super::*;

    fn complete(
        state: &mut AppState,
        target: Target,
        request_id: RequestId,
        path: Option<&str>,
        filter: PreviewFileFilter,
        files: &[&str],
        lines: &[&str],
    ) {
        let _ = update(
            state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                request_id,
                path: path.map(str::to_owned),
                filter,
                files: files.iter().map(ToString::to_string).collect(),
                lines: lines.iter().map(ToString::to_string).collect(),
            }),
        );
    }

    #[test]
    fn finder_request_identity_rejects_an_all_changed_all_aba_completion() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let first_all = state.preview_overlay().unwrap().request_id();
        complete(
            &mut state,
            target,
            first_all,
            None,
            PreviewFileFilter::All,
            &["first"],
            &[],
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        let changed = state.preview_overlay().unwrap().request_id();
        assert_ne!(changed, first_all);
        let _ = update(&mut state, AppEvent::Key(AppKey::Left));
        let latest_all = state.preview_overlay().unwrap().request_id();
        assert_ne!(latest_all, first_all);

        complete(
            &mut state,
            target,
            first_all,
            None,
            PreviewFileFilter::All,
            &["stale"],
            &[],
        );
        let overlay = state.preview_overlay().unwrap();
        assert!(overlay.is_loading());
        assert!(overlay.visible_files().is_empty());

        complete(
            &mut state,
            target,
            latest_all,
            None,
            PreviewFileFilter::All,
            &["latest"],
            &[],
        );
        assert_eq!(
            state.preview_overlay().unwrap().selected_file(),
            Some("latest")
        );
    }

    #[test]
    fn document_search_moves_matches_and_offsets_the_projected_anchor() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let request_id = state.preview_overlay().unwrap().request_id();
        complete(
            &mut state,
            target,
            request_id,
            None,
            PreviewFileFilter::All,
            &["file"],
            &[],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let request_id = state.preview_overlay().unwrap().request_id();
        complete(
            &mut state,
            target,
            request_id,
            Some("file"),
            PreviewFileFilter::All,
            &[],
            &["needle then needle", "last needle"],
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::Char('/')));
        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("needle".into())));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.preview_overlay().unwrap().search_matches().len(), 3);
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('n')));
        assert_eq!(state.preview_overlay().unwrap().current_match(), 1);
        assert_eq!(
            state
                .preview_overlay()
                .unwrap()
                .projected_scroll(Some(10), 20),
            10
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(
            state
                .preview_overlay()
                .unwrap()
                .projected_scroll(Some(10), 20),
            11
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        assert_eq!(
            state
                .preview_overlay()
                .unwrap()
                .projected_scroll(Some(10), 20),
            9
        );
    }

    #[test]
    fn search_match_inventory_and_inputs_are_bounded() {
        let workspace = WorkspaceId::new();
        let mut overlay = PreviewOverlay::loading(Target::Root(workspace));
        overlay.path = Some("file".to_owned());
        overlay.lines = vec!["x".repeat(MAX_PREVIEW_SEARCH_MATCHES + 1)];
        overlay.search = "x".to_owned();
        assert_eq!(overlay.search_matches().len(), MAX_PREVIEW_SEARCH_MATCHES);

        overlay.search_editing = true;
        update_preview_search(
            &mut overlay,
            &AppKey::Paste(format!(
                "{}\u{1b}",
                "z".repeat(MAX_PREVIEW_SEARCH_CHARS + 5)
            )),
        );
        assert_eq!(overlay.search.chars().count(), MAX_PREVIEW_SEARCH_CHARS);
        assert!(!overlay.search.contains('\u{1b}'));

        overlay.search = "missing".to_owned();
        overlay.move_search_match(true);
        assert_eq!(overlay.current_match(), 0);
        let unchanged = overlay.clone();
        update_preview_search(&mut overlay, &AppKey::Up);
        assert_eq!(overlay, unchanged);

        overlay.search = "a👩‍💻".to_owned();
        update_preview_search(&mut overlay, &AppKey::Backspace);
        assert_eq!(overlay.search, "a");
    }

    #[test]
    fn filter_cycles_and_character_search_cover_each_direction() {
        assert_eq!(PreviewFileFilter::Tracked.next(), PreviewFileFilter::All);
        assert_eq!(
            PreviewFileFilter::All.previous(),
            PreviewFileFilter::Tracked
        );
        assert_eq!(
            PreviewFileFilter::Tracked.previous(),
            PreviewFileFilter::Changed
        );

        let mut overlay = PreviewOverlay::loading(Target::Root(WorkspaceId::new()));
        overlay.path = Some("file".to_owned());
        overlay.lines = vec!["n n".to_owned()];
        assert!(update_preview_document(&mut overlay, &AppKey::Char('/')).is_empty());
        update_preview_search(&mut overlay, &AppKey::Char('n'));
        update_preview_search(&mut overlay, &AppKey::Enter);
        assert_eq!(overlay.search(), "n");
        assert!(!overlay.is_search_editing());

        assert!(update_preview_document(&mut overlay, &AppKey::Char('N')).is_empty());
        assert_eq!(overlay.current_match(), 1);
    }

    #[test]
    fn missing_preview_state_closes_the_overlay_and_cancels_work() {
        let mut state = AppState::home(WorkspaceId::new(), Vec::new());
        state.overlay = Some(Overlay::Preview);
        assert_eq!(
            update_preview_overlay(&mut state, &AppKey::Down),
            [Effect::CancelPreview]
        );
        assert_eq!(state.overlay, None);
    }
}
