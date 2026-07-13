//! Pull request modal（PR ポップアップ）。
//!
//! workspace のセッションで見つかった Pull Request を一覧し、選んだ PR の詳細（番号・状態・
//! URL）を見る中央モーダル。↑↓ で選ぶ。中央に浮かぶ枠付きダイアログとして描く（枠・配置は
//! 共通の [`modal`] widget に委譲）。
//!
//! 一覧する PR は core domain の [`PrLink`] を持つ。状態 [`PrModal`] は端末 IO を持たない
//! 純粋な値で、[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。キー入力の
//! 解釈は入力層が整うときに載せ、ここではカーソル移動の純粋操作だけを公開する。

use usagi_core::domain::pullrequest::{PrLink, PrState};

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 58;
/// 一度に表示する Pull Request の最大数。
const MAX_VISIBLE: usize = 6;
const BODY_HEIGHT: usize = 14;

/// PR ポップアップの状態。workspace で見つかった PR 一覧と、その上のカーソルを持つ。
#[derive(Debug, Clone)]
pub struct PrModal {
    prs: Vec<PrLink>,
    selected: usize,
}

/// ダミーの [`PrLink`] を 1 件組む。
fn dummy_pr(number: u32, url: &str, title: &str, state: PrState) -> PrLink {
    let mut pr = PrLink::new(number, url);
    pr.title = Some(title.to_string());
    pr.state = state;
    pr
}

impl PrModal {
    /// デモ用のダミー PR 一覧（open 2 件・merged 1 件）。
    #[must_use]
    pub fn dummy() -> Self {
        Self::new(vec![
            dummy_pr(
                812,
                "https://github.com/kkyosuke/usagi/pull/812",
                "feat(tui): workspace 画面を実装する",
                PrState::Open,
            ),
            dummy_pr(
                809,
                "https://github.com/kkyosuke/usagi/pull/809",
                "feat(tui): new 画面を実装する",
                PrState::Open,
            ),
            dummy_pr(
                801,
                "https://github.com/kkyosuke/usagi/pull/801",
                "feat(tui): config 画面を実装する",
                PrState::Merged,
            ),
        ])
    }

    /// 与えた PR 一覧で開く。先頭を選択する。
    #[must_use]
    pub fn new(prs: Vec<PrLink>) -> Self {
        Self { prs, selected: 0 }
    }

    /// PR 一覧。
    #[must_use]
    pub fn prs(&self) -> &[PrLink] {
        &self.prs
    }

    /// 選択中の添字。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// 選択中の PR。一覧が空なら `None`。
    #[must_use]
    pub fn selected_pr(&self) -> Option<&PrLink> {
        self.prs.get(self.selected)
    }

    /// 選択を次へ（末尾で先頭へ回り込む）。一覧が空なら何もしない。
    pub fn select_next(&mut self) {
        if !self.prs.is_empty() {
            self.selected = (self.selected + 1) % self.prs.len();
        }
    }

    /// 選択を前へ（先頭で末尾へ回り込む）。一覧が空なら何もしない。
    pub fn select_prev(&mut self) {
        if !self.prs.is_empty() {
            self.selected = (self.selected + self.prs.len() - 1) % self.prs.len();
        }
    }
}

/// PR の状態のラベルと色（open=success / merged=feature / dismissed=dim）。
fn state_label(state: PrState) -> (&'static str, Style) {
    match state {
        PrState::Open => ("open", Role::Success.style()),
        PrState::Merged => ("merged", Role::Feature.style()),
        PrState::Dismissed => ("dismissed", Style::new().dim()),
    }
}

/// 1 PR 行: 選択中は `›` マーカー、`#番号`（warning）、状態バッジ、タイトル。幅に切り詰める。
fn pr_row(pr: &PrLink, selected: bool, inner: usize) -> String {
    let marker = if selected {
        Role::Danger.style().bold().paint("›")
    } else {
        " ".to_string()
    };
    let number = Role::Warning
        .style()
        .bold()
        .paint(&format!("#{:<5}", pr.number));
    let (label, style) = state_label(pr.state);
    let badge = style.paint(&format!("{label:<10}"));
    let title = pr.title.as_deref().unwrap_or("(no title)");
    widgets::clip_to_width(&format!("  {marker} {number} {badge} {title}"), inner)
}

/// 選択中 PR の詳細ブロック（状態・URL）。
fn detail_lines(pr: &PrLink) -> Vec<String> {
    let (label, style) = state_label(pr.state);
    vec![
        format!(
            "  {} {}",
            Role::Warning
                .style()
                .bold()
                .paint(&format!("#{}", pr.number)),
            style.paint(label),
        ),
        Style::new().dim().paint(&format!("  {}", pr.url)),
    ]
}

/// 選択行が必ず収まる PR 一覧 viewport の半開区間 `(start, end)`。
fn visible_bounds(state: &PrModal) -> (usize, usize) {
    let len = state.prs.len();
    let visible = len.min(MAX_VISIBLE);
    let start = state
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(len.saturating_sub(visible));
    (start, start + visible)
}

/// PR ポップアップのボディ（枠の内側の行）: 一覧・選択中の詳細・フッタ。
fn body(state: &PrModal) -> Vec<String> {
    let mut lines = vec![Style::new().dim().paint("  Pull requests")];
    if let Some(selected) = state.selected_pr() {
        let (start, end) = visible_bounds(state);
        if start > 0 {
            lines.push(Style::new().dim().paint(&format!("  ↑ {start} more")));
        }
        for (i, pr) in state.prs[start..end].iter().enumerate() {
            let index = start + i;
            lines.push(pr_row(pr, index == state.selected, INNER_WIDTH));
        }
        if end < state.prs.len() {
            lines.push(
                Style::new()
                    .dim()
                    .paint(&format!("  ↓ {} more", state.prs.len() - end)),
            );
        }
        lines.push(String::new());
        lines.extend(detail_lines(selected));
    } else {
        lines.push(String::new());
        lines.push(Style::new().dim().paint("  no pull requests"));
    }
    lines.push(String::new());
    lines.push(Style::new().dim().paint("  ↑↓ select   Esc: close"));
    modal::fixed_body(lines, BODY_HEIGHT)
}

/// 生の端末サイズに対する pull request modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠と中央寄せは [`modal::render_modal`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &PrModal) -> Vec<String> {
    modal::render_modal(
        raw_height,
        raw_width,
        "Pull Request",
        INNER_WIDTH,
        &body(state),
    )
}

/// `base` の workspace フレームを背景に残し、pull request modal を中央に合成する。
/// サイズ 0 は 80×24 にフォールバックする。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &PrModal,
) -> Vec<String> {
    let (height, _) = widgets::normalize_size(raw_height, raw_width);
    // Preserve a sliver of the workspace behind this fixed-height overlay on
    // small terminals; normal terminals retain the complete reserved body.
    let body = modal::fixed_body(body(state), BODY_HEIGHT.min(height.saturating_sub(4)));
    modal::render_over(
        raw_height,
        raw_width,
        base,
        "Pull Request",
        INNER_WIDTH,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::{PrModal, render, render_over};
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::pullrequest::PrLink;

    #[test]
    fn empty_and_populated_lists_keep_the_pr_box_height_stable() {
        let empty = render(40, 80, &PrModal::new(Vec::new()))
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        let populated = render(40, 80, &PrModal::dummy())
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert_eq!(empty, populated);
    }

    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn joined(state: &PrModal) -> String {
        render(24, 80, state)
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dummy_lists_pull_requests() {
        let modal = PrModal::dummy();
        assert_eq!(modal.prs().len(), 3);
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.selected_pr().map(|p| p.number), Some(812));
        // derive された Clone / Debug も触れる。
        assert!(format!("{:?}", modal.clone()).contains("812"));
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut modal = PrModal::dummy();
        modal.select_prev(); // wrap to last (index 2 = #801)
        assert_eq!(modal.selected(), 2);
        assert_eq!(modal.selected_pr().map(|p| p.number), Some(801));
        modal.select_next(); // wrap to 0
        assert_eq!(modal.selected(), 0);
    }

    #[test]
    fn selection_is_a_noop_when_empty() {
        let mut modal = PrModal::new(Vec::new());
        assert!(modal.selected_pr().is_none());
        modal.select_next();
        modal.select_prev();
        assert_eq!(modal.selected(), 0);
    }

    #[test]
    fn long_lists_scroll_to_keep_the_selection_and_footer_visible() {
        let prs = (1..=10)
            .map(|number| PrLink::new(number, format!("https://example.com/pull/{number}")))
            .collect();
        let mut modal = PrModal::new(prs);
        for _ in 0..8 {
            modal.select_next();
        }

        let text = joined(&modal);
        assert!(text.contains("#9"));
        assert!(text.contains("↑ 3 more"));
        assert!(text.contains("↓ 1 more"));
        assert!(!text.contains("#1 "));
        assert!(text.contains("Esc: close"));

        modal.select_next();
        let last = joined(&modal);
        assert!(last.contains("#10"));
        assert!(last.contains("↑ 4 more"));
        assert!(!last.contains("↓ 1 more"));
    }

    #[test]
    fn render_lists_prs_with_state_and_shows_the_selected_detail() {
        let text = joined(&PrModal::dummy());
        assert!(text.contains("Pull Request")); // タイトル
        assert!(text.contains("Pull requests")); // 見出し
        assert!(text.contains("#812"));
        assert!(text.contains("open"));
        assert!(text.contains("merged")); // #801 は merged
        assert!(text.contains("workspace 画面")); // タイトル
        // 選択中 PR の URL が詳細に出る。
        assert!(text.contains("github.com/kkyosuke/usagi/pull/812"));
        assert!(text.contains("Esc: close"));
        assert!(text.contains('›')); // 選択マーカー
    }

    #[test]
    fn render_reflects_the_selected_pr_detail() {
        let mut modal = PrModal::dummy();
        modal.select_prev(); // #801（merged）を選択
        let text = joined(&modal);
        assert!(text.contains("github.com/kkyosuke/usagi/pull/801"));
    }

    #[test]
    fn render_handles_a_missing_title() {
        // タイトル無しの PR は "(no title)" を出す。
        let modal = PrModal::new(vec![PrLink::new(7, "https://example.com/pull/7")]);
        let text = joined(&modal);
        assert!(text.contains("#7"));
        assert!(text.contains("(no title)"));
    }

    #[test]
    fn render_shows_an_empty_notice() {
        let text = joined(&PrModal::new(Vec::new()));
        assert!(text.contains("no pull requests"));
    }

    #[test]
    fn render_labels_a_dismissed_pr() {
        use usagi_core::domain::pullrequest::PrState;
        let mut pr = PrLink::new(3, "https://example.com/pull/3");
        pr.state = PrState::Dismissed;
        let text = joined(&PrModal::new(vec![pr]));
        assert!(text.contains("dismissed"));
    }

    #[test]
    fn render_fills_the_terminal() {
        let frame = render(24, 80, &PrModal::dummy());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| display_width(l) <= 80));
        // サイズ 0 は 80×24 にフォールバック。
        assert_eq!(render(0, 0, &PrModal::dummy()).len(), 24);
    }

    #[test]
    fn render_over_keeps_the_workspace_background_visible() {
        let base: Vec<String> = (0..24)
            .map(|row| format!("workspace-row-{row}-{}", ".".repeat(80)))
            .collect();
        let frame = render_over(24, 80, &base, &PrModal::dummy());
        let text = frame.join("\n");

        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 80));
        assert!(frame[0].starts_with("workspace-row-0-"));
        assert!(text.contains("Pull Request"));
        assert!(text.contains("#812"));
        let modal_row = frame.iter().find(|line| line.contains('┌')).unwrap();
        assert!(modal_row.starts_with("workspace"));
        assert!(modal_row.trim_end().ends_with('.'));
    }

    #[test]
    fn render_over_fits_ansi_cjk_background_on_a_narrow_terminal() {
        let base = vec![format!("\u{1b}[36m{}\u{1b}[0m", "背景".repeat(8)); 16];
        let frame = render_over(16, 9, &base, &PrModal::new(Vec::new()));

        assert_eq!(frame.len(), 16);
        assert!(frame.iter().all(|line| display_width(line) == 9));
        assert!(frame.iter().any(|line| line.contains('┌')));
        assert!(frame.iter().any(|line| line.contains("\u{1b}[36m")));
    }
}
