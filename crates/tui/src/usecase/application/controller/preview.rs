//! Pure finder/document state and reducer for the Home Preview overlay.

use usagi_core::domain::presentation_text::presentation_character_is_safe;

use crate::usecase::fuzzy::fuzzy_score;

use super::{AppKey, AppState, Effect, SafeError, Target};

const MAX_PREVIEW_FILTER_CHARS: usize = 256;

/// Selected-session file finder and read-only text preview state.
///
/// Repository-relative paths and file lines return through [`Effect::LoadPreview`].
/// The reducer owns filtering, selection, the finder/document transition, and
/// document scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewOverlay {
    pub(super) target: Target,
    pub(super) files: Vec<String>,
    pub(super) filter: String,
    pub(super) selected: usize,
    pub(super) path: Option<String>,
    pub(super) lines: Vec<String>,
    pub(super) scroll: usize,
    pub(super) loading: bool,
    pub(super) error: Option<SafeError>,
}

impl PreviewOverlay {
    pub(super) fn loading(target: Target) -> Self {
        Self {
            target,
            files: Vec::new(),
            filter: String::new(),
            selected: 0,
            path: None,
            lines: Vec::new(),
            scroll: 0,
            loading: true,
            error: None,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// Current fuzzy filter.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
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
    /// 現在の先頭行 offset。
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
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
}

/// Preview overlay input. The finder accepts a fuzzy filter and opens its
/// selected file; the document scrolls and returns to the cached finder on Esc.
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
        let overlay = state.preview_overlay.as_mut().unwrap();
        match key {
            AppKey::Escape => {
                overlay.path = None;
                overlay.lines.clear();
                overlay.scroll = 0;
                overlay.loading = false;
                overlay.error = None;
                return vec![Effect::CancelPreview];
            }
            AppKey::Up => overlay.scroll = overlay.scroll.saturating_sub(1),
            AppKey::Down => overlay.scroll = overlay.scroll.saturating_add(1),
            _ => {}
        }
        return Vec::new();
    }

    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.preview_overlay = None;
            return vec![Effect::CancelPreview];
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
            overlay.filter.pop();
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
            overlay.loading = true;
            overlay.error = None;
            return vec![Effect::LoadPreview {
                target,
                path: Some(path),
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
