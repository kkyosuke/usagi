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
    footer: String,
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
            footer: "↑↓ scroll   Esc: close".to_owned(),
        }
    }

    /// Mark this overlay as an acknowledgement dialog. Its owner closes it on
    /// the next user input instead of exposing scroll controls.
    #[must_use]
    pub fn acknowledgement(mut self) -> Self {
        self.dismiss_on_any_key = true;
        self
    }

    /// Replace the default viewer controls with context-specific hints.
    #[must_use]
    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = footer.into();
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

    fn body(&self, body_height: usize) -> Vec<String> {
        let lines = self.lines();
        // border 2, status 1, footer 1 を先に確保する。極小 terminal でも 1 行だけは
        // viewport に残し、render_modal / render_over が最終 clip を担う。
        // scroll indicator（最大 2 行）と footer（空行を含め 2 行）および枠を
        // 先に差し引く。これにより通常サイズでは footer が clip されない。
        let viewport = body_height.saturating_sub(4).max(1);
        // text-viewer shape: offset-anchored viewport + shared `↑/↓ N more`
        // scroll rendering, the same emission the PR list uses.
        let (start, end) = modal::viewport_window(lines.len(), self.scroll, viewport);
        let mut body = modal::scroll_window(&lines, start, end);
        body.push(String::new());
        body.push(modal::footer(if self.dismiss_on_any_key {
            "Press any key to close"
        } else {
            &self.footer
        }));
        modal::fixed_body(body, body_height)
    }

    /// Compose into a body that already fits inside the terminal height.
    ///
    /// Unlike the legacy renderers below, a reserved frame is not vertically
    /// clipped after composition. On a body shorter than five rows, prioritize
    /// one document row and the footer over showing every scroll indicator so
    /// the reader never loses its way back to the finder.
    fn body_with_reserved_footer(&self, body_height: usize) -> Vec<String> {
        if body_height >= 5 {
            return self.body(body_height);
        }

        let lines = self.lines();
        let (start, end) = modal::viewport_window(lines.len(), self.scroll, 1);
        let line = lines[start].clone();
        let footer = modal::footer(if self.dismiss_on_any_key {
            "Press any key to close"
        } else {
            &self.footer
        });
        match body_height {
            0 => Vec::new(),
            1 => vec![line],
            2 => vec![line, footer],
            3 => vec![line, String::new(), footer],
            _ => {
                let mut body = if start > 0 {
                    vec![modal::scroll_above(start), line]
                } else if end < lines.len() {
                    vec![line, modal::scroll_below(lines.len() - end)]
                } else {
                    vec![line]
                };
                body.push(String::new());
                body.push(footer);
                modal::fixed_body(body, body_height)
            }
        }
    }
}

/// 空の画面に中央配置して描く。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &TextOverlay) -> Vec<String> {
    let (height, _) = crate::presentation::widgets::normalize_size(raw_height, raw_width);
    let body_height = BODY_HEIGHT.min(height.saturating_sub(2));
    modal::render_modal(
        raw_height,
        raw_width,
        &state.title,
        INNER_WIDTH,
        &state.body(body_height),
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
    let body_height = BODY_HEIGHT.min(height.saturating_sub(2));
    modal::render_over(
        raw_height,
        raw_width,
        base,
        &state.title,
        INNER_WIDTH,
        &state.body(body_height),
    )
}

/// Render a text overlay with caller-selected preferred dimensions.
///
/// File Preview uses this to give both its finder and document stages one large,
/// stable frame. Other text overlays retain the compact default above.
#[must_use]
pub(crate) fn render_over_with_layout(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &TextOverlay,
    inner_width: usize,
    desired_body_height: usize,
) -> Vec<String> {
    let body_height = modal::reserved_body_height(raw_height, raw_width, desired_body_height);
    modal::render_over(
        raw_height,
        raw_width,
        base,
        &state.title,
        inner_width,
        &state.body_with_reserved_footer(body_height),
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
        // Scroll indicators use the shared `↑/↓ N more` wording (unified with
        // the PR list; the viewer previously said `↑ N lines`).
        assert!(text.contains("↑ 12 more"));
        assert!(text.contains("more"));
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
    fn reserved_body_keeps_its_footer_on_a_short_terminal() {
        let modal = TextOverlay::new(
            "Preview",
            OverlayDocument::Ready(vec!["first".into(), "selected".into(), "last".into()]),
        )
        .scrolled_to(1)
        .with_footer("Esc: back to files");

        assert!(modal.body_with_reserved_footer(0).is_empty());
        assert_eq!(modal.body_with_reserved_footer(1).len(), 1);
        assert_eq!(modal.body_with_reserved_footer(2).len(), 2);
        assert_eq!(modal.body_with_reserved_footer(3).len(), 3);
        let body = modal.body_with_reserved_footer(4);
        assert_eq!(body.len(), 4);
        assert!(body.join("\n").contains("selected"));
        assert!(body.join("\n").contains("Esc: back to files"));

        let at_start = modal.clone().scrolled_to(0).body_with_reserved_footer(4);
        assert!(at_start.join("\n").contains("↓ 2 more"));
        let only_line = TextOverlay::new("Preview", OverlayDocument::Ready(vec!["only".into()]))
            .body_with_reserved_footer(4);
        assert!(only_line.join("\n").contains("only"));
        assert_eq!(modal.body_with_reserved_footer(5).len(), 5);
        assert!(
            modal
                .acknowledgement()
                .body_with_reserved_footer(2)
                .join("\n")
                .contains("Press any key to close")
        );
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
