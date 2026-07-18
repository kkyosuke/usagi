//! Preview、diff、note などの長文を表示する scrollable overlay。
//!
//! データ取得は呼び出し側の port に委ねる。この view は安全に表示できる行だけを
//! 受け取り、狭い端末では背景を残したまま本文と枠を clip する。

use crate::presentation::theme::Style;
use crate::presentation::widgets::modal;

/// 長文 overlay の希望する内側幅。
pub const INNER_WIDTH: usize = 68;
const BODY_HEIGHT: usize = 14;

/// overlay に表示する安全な document。`message` は backend の生エラーを含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayDocument {
    /// 表示できる本文。
    Ready(Vec<String>),
    /// データが無い、または backend が安全な要約だけを返した場合の fallback。
    Unavailable(String),
}

/// 長文をスクロールして表示する modal の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextOverlay {
    title: String,
    document: OverlayDocument,
    scroll: usize,
    dismiss_on_any_key: bool,
}

impl TextOverlay {
    /// タイトルと安全な document から overlay を作る。
    #[must_use]
    pub fn new(title: impl Into<String>, document: OverlayDocument) -> Self {
        Self {
            title: title.into(),
            document,
            scroll: 0,
            dismiss_on_any_key: false,
        }
    }

    /// Mark this overlay as an acknowledgement dialog. Its owner closes it on
    /// the next user input instead of exposing scroll controls.
    #[must_use]
    pub fn acknowledgement(mut self) -> Self {
        self.dismiss_on_any_key = true;
        self
    }

    /// 現在の先頭行 offset。
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Open at a caller-owned scroll offset. The controller [`Overlay::Preview`]
    /// owns the scroll, so `render_home` rebuilds the overlay at that offset each
    /// frame instead of mutating an overlay-local cursor.
    ///
    /// [`Overlay::Preview`]: crate::usecase::application::controller::Overlay::Preview
    #[must_use]
    pub fn scrolled_to(mut self, offset: usize) -> Self {
        self.scroll = offset;
        self
    }

    /// 1 行上へ移動する。
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// 1 行下へ移動する。最終行より下には進まない。
    pub fn scroll_down(&mut self) {
        self.scroll = self
            .scroll
            .saturating_add(1)
            .min(self.lines().len().saturating_sub(1));
    }

    fn lines(&self) -> Vec<String> {
        match &self.document {
            OverlayDocument::Ready(lines) if !lines.is_empty() => lines.clone(),
            OverlayDocument::Ready(_) => vec![Style::new().dim().paint("No content available.")],
            OverlayDocument::Unavailable(message) => vec![Style::new().dim().paint(message)],
        }
    }

    fn body(&self, height: usize) -> Vec<String> {
        let lines = self.lines();
        // border 2, status 1, footer 1 を先に確保する。極小 terminal でも 1 行だけは
        // viewport に残し、render_modal / render_over が最終 clip を担う。
        // scroll indicator（最大 2 行）と footer（空行を含め 2 行）および枠を
        // 先に差し引く。これにより通常サイズでは footer が clip されない。
        let body_height = BODY_HEIGHT.min(height.saturating_sub(2));
        let viewport = body_height.saturating_sub(4).max(1);
        let start = self.scroll.min(lines.len().saturating_sub(1));
        let end = start.saturating_add(viewport).min(lines.len());
        let mut body = Vec::new();
        if start > 0 {
            body.push(Style::new().dim().paint(&format!("↑ {start} lines")));
        }
        body.extend(lines[start..end].iter().cloned());
        if end < lines.len() {
            body.push(
                Style::new()
                    .dim()
                    .paint(&format!("↓ {} more", lines.len() - end)),
            );
        }
        body.push(String::new());
        body.push(Style::new().dim().paint(if self.dismiss_on_any_key {
            "Press any key to close"
        } else {
            "↑↓ scroll   Esc: close"
        }));
        modal::fixed_body(body, body_height)
    }
}

/// 空の画面に中央配置して描く。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &TextOverlay) -> Vec<String> {
    let (height, _) = crate::presentation::widgets::normalize_size(raw_height, raw_width);
    modal::render_modal(
        raw_height,
        raw_width,
        &state.title,
        INNER_WIDTH,
        &state.body(height),
    )
}

/// `base` を背景に残して中央配置して描く。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &TextOverlay,
) -> Vec<String> {
    let (height, _) = crate::presentation::widgets::normalize_size(raw_height, raw_width);
    modal::render_over(
        raw_height,
        raw_width,
        base,
        &state.title,
        INNER_WIDTH,
        &state.body(height),
    )
}

#[cfg(test)]
mod tests {
    use super::{OverlayDocument, TextOverlay, render, render_over};
    use crate::presentation::widgets::display_width;

    #[test]
    fn long_text_scrolls_without_losing_the_footer() {
        let mut modal = TextOverlay::new(
            "Preview",
            OverlayDocument::Ready((0..20).map(|n| format!("line {n}")).collect()),
        );
        for _ in 0..12 {
            modal.scroll_down();
        }
        let text = render(10, 40, &modal).join("\n");
        assert_eq!(modal.scroll(), 12);
        assert!(text.contains("line 12"));
        assert!(!text.contains("line 0"));
        assert!(text.contains("Esc: close"));
        modal.scroll_up();
        assert_eq!(modal.scroll(), 11);
    }

    #[test]
    fn scrolled_to_opens_at_a_caller_owned_offset() {
        let modal = TextOverlay::new(
            "Preview",
            OverlayDocument::Ready(vec!["a".into(), "b".into(), "c".into()]),
        )
        .scrolled_to(2);
        assert_eq!(modal.scroll(), 2);
        assert!(render(10, 40, &modal).join("\n").contains('c'));
    }

    #[test]
    fn fallback_and_tiny_sizes_are_safe() {
        let modal = TextOverlay::new(
            "Diff",
            OverlayDocument::Unavailable("Diff data is unavailable.".to_string()),
        );
        assert!(render(24, 80, &modal).join("\n").contains("unavailable"));
        let empty = TextOverlay::new("Preview", OverlayDocument::Ready(Vec::new()));
        assert!(
            render(24, 80, &empty)
                .join("\n")
                .contains("No content available.")
        );
        let base = vec!["background".to_string(); 3];
        let frame = render_over(3, 3, &base, &modal);
        assert_eq!(frame.len(), 3);
        assert!(frame.iter().all(|line| display_width(line) <= 3));
        assert!(frame.join("\n").contains("bac"));
    }

    #[test]
    fn ready_and_fallback_documents_keep_the_overlay_height_stable() {
        let ready = TextOverlay::new("Preview", OverlayDocument::Ready(vec!["body".into()]));
        let fallback = TextOverlay::new(
            "Preview",
            OverlayDocument::Unavailable("unavailable".into()),
        );
        let box_height = |modal: &TextOverlay| {
            render(24, 80, modal)
                .iter()
                .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
                .count()
        };
        assert_eq!(box_height(&ready), box_height(&fallback));
    }

    #[test]
    fn overlay_keeps_the_background_visible() {
        let modal = TextOverlay::new("Notes", OverlayDocument::Ready(vec!["hello".to_string()]));
        let base: Vec<String> = (0..24)
            .map(|row| format!("workspace-{row}-{}", ".".repeat(70)))
            .collect();
        let frame = render_over(24, 80, &base, &modal);
        assert!(frame[0].starts_with("workspace-0-"));
        assert!(frame.join("\n").contains("Notes"));
        assert!(frame.iter().all(|line| display_width(line) == 80));
    }

    #[test]
    fn acknowledgement_uses_an_any_key_dismiss_hint() {
        let modal = TextOverlay::new("Error", OverlayDocument::Ready(vec!["failed".into()]))
            .acknowledgement();

        assert!(
            render(24, 80, &modal)
                .join("\n")
                .contains("Press any key to close")
        );
    }
}
