//! Pull request modal（PR ポップアップ）。
//!
//! workspace のセッションで見つかった Pull Request を repository ごとに一覧し、番号・状態・
//! title・remote status を見る中央モーダル。←→ で status tab、↑↓ で PR を選ぶ。中央に浮かぶ
//! 枠付きダイアログとして描く（枠・配置は共通の [`modal`] widget に委譲）。
//!
//! 一覧する PR は daemon inventory の canonical [`PrEntry`] を持つ。状態 [`PrModal`] は端末 IO を持たない
//! 純粋な値で、[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。キー入力の
//! 解釈は入力層が整うときに載せ、ここではカーソル移動の純粋操作だけを公開する。

use usagi_core::domain::pr_inventory::{
    PrChecksState, PrEntry, PrRefreshState, PrReviewDecision, PrState, canonicalize,
};

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;
use crate::usecase::application::controller::PrFilter;

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 88;
/// 一度に表示する Pull Request の最大数。
const MAX_VISIBLE: usize = 8;
const BODY_HEIGHT: usize = 15;
const TAB_HEIGHT: usize = 1;
const FOOTER_HEIGHT: usize = 2;
const MAX_GAP_HEIGHT: usize = 2;

/// PR ポップアップの状態。workspace で見つかった PR 一覧と、その上のカーソルを持つ。
#[derive(Debug, Clone)]
pub struct PrModal {
    prs: Vec<PrEntry>,
    selected: usize,
    filter: PrFilter,
}

/// ダミーの [`PrEntry`] を 1 件組む。
fn dummy_pr(url: &str, title: &str, state: PrState) -> PrEntry {
    let mut pr = PrEntry::new(canonicalize(url).expect("dummy PR URL is canonical"));
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
                "https://github.com/kkyosuke/usagi/pull/812",
                "feat(tui): workspace 画面を実装する",
                PrState::Open,
            ),
            dummy_pr(
                "https://github.com/kkyosuke/usagi/pull/809",
                "feat(tui): new 画面を実装する",
                PrState::Open,
            ),
            dummy_pr(
                "https://github.com/kkyosuke/usagi/pull/801",
                "feat(tui): config 画面を実装する",
                PrState::Merged,
            ),
        ])
    }

    /// 与えた PR 一覧で開く。先頭を選択する。
    #[must_use]
    pub fn new(prs: Vec<PrEntry>) -> Self {
        Self {
            prs,
            selected: 0,
            filter: PrFilter::All,
        }
    }

    /// Open with a caller-owned cursor, clamped to the list. The controller
    /// [`Overlay::Prs`] owns the selection, so `render_home` rebuilds the modal
    /// at that index each frame instead of mutating a modal-local cursor.
    ///
    /// [`Overlay::Prs`]: crate::usecase::application::controller::Overlay::Prs
    #[must_use]
    pub fn with_selection(prs: Vec<PrEntry>, selected: usize) -> Self {
        let selected = selected.min(prs.len().saturating_sub(1));
        Self {
            prs,
            selected,
            filter: PrFilter::All,
        }
    }

    #[must_use]
    pub const fn with_filter(mut self, filter: PrFilter) -> Self {
        self.filter = filter;
        self
    }

    /// PR 一覧。
    #[must_use]
    pub fn prs(&self) -> &[PrEntry] {
        &self.prs
    }

    /// 選択中の添字。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// 選択中の PR。一覧が空なら `None`。
    #[must_use]
    pub fn selected_pr(&self) -> Option<&PrEntry> {
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
fn state_label(pr: &PrEntry) -> (&'static str, Style) {
    match pr.state {
        PrState::Open => ("open", Role::Success.style()),
        PrState::Merged => ("merged", Role::Feature.style()),
        PrState::Closed => ("closed", Style::new().dim()),
        PrState::Dismissed => ("dismissed", Style::new().dim()),
    }
}

fn repository(url: &str) -> &str {
    url.strip_prefix("https://github.com/")
        .and_then(|path| path.split_once("/pull/"))
        .map_or("unknown/unknown", |(repository, _)| repository)
}

fn remote_summary(pr: &PrEntry) -> String {
    let mut parts = Vec::new();
    if pr.draft {
        parts.push("draft");
    }
    parts.push(match pr.checks {
        Some(PrChecksState::Passing) => "ci✓",
        Some(PrChecksState::Failing) => "ci✗",
        Some(PrChecksState::Pending) => "ci…",
        None => "ci–",
    });
    if let Some(review) = pr.review {
        parts.push(match review {
            PrReviewDecision::Approved => "review✓",
            PrReviewDecision::ChangesRequested => "changes",
            PrReviewDecision::ReviewRequired => "review…",
        });
    }
    parts.join(" ")
}

/// 1 PR 行: 選択中は `›` マーカー、`#番号`（warning）、状態バッジ、タイトル。幅に切り詰める。
fn pr_row(pr: &PrEntry, selected: bool, inner: usize) -> String {
    let marker = modal::selection_marker(selected);
    let number = Role::Warning
        .style()
        .bold()
        .paint(&format!("#{:<5}", pr.number()));
    let (label, style) = state_label(pr);
    let badge = style.paint(&format!("{label:<10}"));
    let title = pr.title.as_deref().unwrap_or("(no title)");
    let hint = if pr.refresh == PrRefreshState::Pending {
        Some("refresh pending")
    } else if pr.refresh == PrRefreshState::BackingOff {
        Some("refresh retrying")
    } else {
        None
    };
    let hint = hint.map_or_else(String::new, |hint| {
        format!("  {}", Style::new().dim().paint(hint))
    });
    let remote = Style::new().dim().paint(&remote_summary(pr));
    modal::content_line(
        &format!("{marker} {number} {badge} {title}  {remote}{hint}"),
        inner,
    )
}

/// Preserve the controller-owned PR order while adding one repository heading
/// before each consecutive group visible in the viewport.
fn grouped_pr_rows(prs: &[PrEntry], start: usize, end: usize, selected: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut previous_repository = None;
    for (index, pr) in prs[start..end].iter().enumerate() {
        let repository = repository(pr.url());
        if previous_repository != Some(repository) {
            rows.push(modal::caption(repository));
            previous_repository = Some(repository);
        }
        let absolute_index = start + index;
        rows.push(pr_row(pr, absolute_index == selected, INNER_WIDTH));
    }
    rows
}

/// Select a PR window whose repository headings and scroll indicators all fit
/// in the fixed list region. PR rows retain priority and the cursor always stays
/// visible; only the number of visible PRs shrinks when several repositories
/// need headings at once.
fn grouped_window(state: &PrModal, list_height: usize) -> Vec<String> {
    let len = state.prs.len();
    let mut capacity = len.min(MAX_VISIBLE).min(list_height);
    loop {
        let (start, end) = modal::list_window(len, state.selected, capacity);
        let rows = grouped_pr_rows(&state.prs, start, end, state.selected);
        let indicators = usize::from(start > 0) + usize::from(end < len);
        if rows.len() + indicators <= list_height {
            let mut visible = Vec::with_capacity(rows.len() + indicators);
            if start > 0 {
                visible.push(modal::scroll_above(start));
            }
            visible.extend(rows);
            if end < len {
                visible.push(modal::scroll_below(len - end));
            }
            return visible;
        }
        if capacity == 1 {
            return vec![pr_row(&state.prs[state.selected], true, INNER_WIDTH)];
        }
        capacity -= 1;
    }
}

fn status_tabs(active: PrFilter) -> String {
    let choices = PrFilter::TABS.map(|filter| {
        let role = match filter {
            PrFilter::All => Role::Accent,
            PrFilter::Open => Role::Success,
            PrFilter::Closed => Role::Warning,
            PrFilter::Merged => Role::Feature,
        };
        (filter.label(), role)
    });
    modal::choice_buttons(active.tab_index(), &choices)
}

/// PR ポップアップのボディ（枠の内側の行）: 一覧とフッタ。
///
/// 選択追従の viewport は [`modal::list_window`] を使い、repository 見出しと
/// `↑/↓ N more` を含めて固定高へ収める。
fn body(state: &PrModal, body_height: usize) -> Vec<String> {
    let footer_height = FOOTER_HEIGHT.min(body_height.saturating_sub(TAB_HEIGHT));
    let flexible = body_height.saturating_sub(TAB_HEIGHT + footer_height);
    let gap_height = MAX_GAP_HEIGHT.min(flexible.saturating_sub(1));
    let list_height = flexible.saturating_sub(gap_height);

    let mut lines = vec![status_tabs(state.filter)];
    if gap_height > 0 {
        lines.push(String::new());
    }
    if list_height > 0 {
        if state.selected_pr().is_some() {
            lines.extend(grouped_window(state, list_height));
        } else {
            lines.push(modal::empty_notice("no pull requests"));
        }
    }
    if gap_height > 1 {
        lines.push(String::new());
    }
    if footer_height > 0 {
        lines.push(modal::footer("←→: status  ↑↓: select"));
    }
    if footer_height > 1 {
        lines.push(modal::footer(
            "c: copy  Ctrl-X: dismiss  Enter: open  Esc: close",
        ));
    }
    lines
}

/// Whether a Home-relative terminal cell is inside the rendered PR modal box.
/// This delegates the exact fixed-body and centring geometry to the shared
/// modal widget used by [`render_over`].
#[must_use]
pub fn contains(raw_height: usize, raw_width: usize, column: u16, row: u16) -> bool {
    modal::body_contains(raw_height, raw_width, INNER_WIDTH, BODY_HEIGHT, column, row)
}

/// 生の端末サイズに対する pull request modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠・中央寄せ・body 予約は [`modal::render_body`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &PrModal) -> Vec<String> {
    modal::render_body(
        raw_height,
        raw_width,
        "Pull Request",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state, BODY_HEIGHT),
    )
}

/// `base` の workspace フレームを背景に残し、pull request modal を中央に合成する。
/// 小端末では [`modal::render_body_over`] が背景の帯を残す。サイズ 0 は 80×24 にフォールバックする。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &PrModal,
) -> Vec<String> {
    let body_height = modal::reserved_body_height(raw_height, raw_width, BODY_HEIGHT);
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        "Pull Request",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state, body_height),
    )
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{PrModal, body, contains, render, render_over, status_tabs};
    use crate::presentation::widgets::{display_width, strip_ansi};
    use crate::usecase::application::controller::PrFilter;
    use usagi_core::domain::pr_inventory::{
        PrChecksState, PrEntry, PrRefreshState, PrReviewDecision, PrState, canonicalize,
    };

    fn pr_entry(number: u64, url: &str) -> PrEntry {
        let identity = canonicalize(url).unwrap_or_else(|| {
            canonicalize(&format!(
                "https://github.com/example/repository/pull/{number}"
            ))
            .unwrap()
        });
        PrEntry::new(identity)
    }

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

    fn joined(state: &PrModal) -> String {
        render(24, 80, state)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dummy_lists_pull_requests() {
        let modal = PrModal::dummy();
        assert_eq!(modal.prs().len(), 3);
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.selected_pr().map(PrEntry::number), Some(812));
        // derive された Clone / Debug も触れる。
        assert!(format!("{:?}", modal.clone()).contains("812"));
    }

    #[test]
    fn daemon_entries_project_every_state_and_refresh_flag() {
        let states = [
            PrState::Open,
            PrState::Closed,
            PrState::Merged,
            PrState::Dismissed,
        ];
        let entries = states
            .into_iter()
            .enumerate()
            .map(|(index, state)| PrEntry {
                identity: canonicalize(&format!("https://github.com/o/r/pull/{}", index + 1))
                    .unwrap(),
                title: Some(format!("PR {index}")),
                state,
                head_oid: None,
                pinned: false,
                refresh: if index == 0 {
                    PrRefreshState::Pending
                } else {
                    PrRefreshState::Idle
                },
                draft: false,
                checks: None,
                review: None,
                auto_open: true,
            })
            .collect::<Vec<_>>();
        let modal = PrModal::new(entries);
        assert_eq!(modal.prs().len(), 4);
        assert_eq!(modal.prs()[0].refresh, PrRefreshState::Pending);
        let rendered = joined(&modal);
        assert!(rendered.contains("open"));
    }

    #[test]
    fn remote_metadata_renders_every_draft_check_and_review_label() {
        let variants = [
            (
                true,
                Some(PrChecksState::Passing),
                Some(PrReviewDecision::Approved),
            ),
            (
                false,
                Some(PrChecksState::Failing),
                Some(PrReviewDecision::ChangesRequested),
            ),
            (
                false,
                Some(PrChecksState::Pending),
                Some(PrReviewDecision::ReviewRequired),
            ),
        ];
        let entries = variants
            .into_iter()
            .enumerate()
            .map(|(index, (draft, checks, review))| PrEntry {
                identity: canonicalize(&format!(
                    "https://github.com/acme/widget/pull/{}",
                    index + 1
                ))
                .unwrap(),
                title: Some(format!("metadata {index}")),
                state: PrState::Open,
                head_oid: None,
                pinned: false,
                refresh: PrRefreshState::Idle,
                draft,
                checks,
                review,
                auto_open: true,
            })
            .collect::<Vec<_>>();

        let rendered = joined(&PrModal::new(entries));
        for label in [
            "draft",
            "ci✓",
            "review✓",
            "ci✗",
            "changes",
            "ci…",
            "review…",
        ] {
            assert!(rendered.contains(label), "missing {label}: {rendered}");
        }
    }

    #[test]
    fn with_selection_clamps_the_cursor_to_the_list() {
        let prs = vec![
            pr_entry(1, "https://example.com/pull/1"),
            pr_entry(2, "https://example.com/pull/2"),
        ];
        let at_second = PrModal::with_selection(prs.clone(), 1);
        assert_eq!(at_second.selected(), 1);
        assert_eq!(at_second.selected_pr().map(PrEntry::number), Some(2));
        // An out-of-range index clamps to the last entry.
        assert_eq!(PrModal::with_selection(prs, 9).selected(), 1);
        // An empty list stays at zero with no selection.
        let empty = PrModal::with_selection(Vec::new(), 3);
        assert_eq!(empty.selected(), 0);
        assert!(empty.selected_pr().is_none());
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut modal = PrModal::dummy();
        modal.select_prev(); // wrap to last (index 2 = #801)
        assert_eq!(modal.selected(), 2);
        assert_eq!(modal.selected_pr().map(PrEntry::number), Some(801));
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
            .map(|number| pr_entry(number, &format!("https://example.com/pull/{number}")))
            .collect();
        let mut modal = PrModal::new(prs);
        for _ in 0..8 {
            modal.select_next();
        }

        let text = joined(&modal);
        assert!(text.contains("#9"));
        assert!(text.contains("↑ 2 more"));
        assert!(text.contains("↓ 1 more"));
        assert!(!text.contains("#1 "));
        assert!(text.contains("Esc: close"));

        modal.select_next();
        let last = joined(&modal);
        assert!(last.contains("#10"));
        assert!(last.contains("↑ 2 more"));
        assert!(!last.contains("↓ 1 more"));
    }

    #[test]
    fn status_tabs_are_always_visible_and_move_the_active_style() {
        let open = status_tabs(PrFilter::Open);
        let closed = status_tabs(PrFilter::Closed);
        let labels = strip_ansi(&open);

        for label in ["all", "open", "closed", "merged"] {
            assert!(labels.contains(label));
        }
        assert_eq!(labels.matches('[').count(), 4);
        assert_ne!(open, closed);

        let text = joined(&PrModal::dummy().with_filter(PrFilter::Merged));
        assert!(text.contains("←→: status  ↑↓: select"));
        assert!(!text.contains("f: filter"));
    }

    #[test]
    fn render_lists_each_pr_once_under_one_modal_title() {
        let text = joined(&PrModal::dummy());
        assert_eq!(text.matches("Pull Request").count(), 1);
        assert!(text.contains("#812"));
        assert_eq!(text.matches("#812").count(), 1);
        assert!(text.contains("open"));
        assert!(text.contains("merged")); // #801 は merged
        assert!(text.contains("workspace 画面")); // タイトル
        assert_eq!(text.matches("kkyosuke/usagi").count(), 1);
        assert!(!text.contains("github.com/kkyosuke/usagi/pull/812"));
        assert!(text.contains("Esc: close"));
        assert!(text.contains('›')); // 選択マーカー
    }

    #[test]
    fn repository_headings_group_prs_without_changing_selection_order() {
        let prs = vec![
            pr_entry(11, "https://github.com/acme/api/pull/11"),
            pr_entry(12, "https://github.com/acme/api/pull/12"),
            pr_entry(21, "https://github.com/acme/web/pull/21"),
        ];
        let text = joined(&PrModal::with_selection(prs, 2));

        assert_eq!(text.matches("acme/api").count(), 1);
        assert_eq!(text.matches("acme/web").count(), 1);
        assert!(
            text.lines()
                .any(|line| line.contains('›') && line.contains("#21"))
        );
        assert_eq!(text.matches("#11").count(), 1);
        assert_eq!(text.matches("#12").count(), 1);
        assert_eq!(text.matches("#21").count(), 1);
    }

    #[test]
    fn modal_hit_test_includes_the_border_but_not_its_background() {
        assert!(contains(24, 80, 0, 2));
        assert!(contains(24, 80, 79, 20));
        assert!(!contains(24, 80, 0, 1));
        assert!(!contains(24, 80, 80, 2));
    }

    #[test]
    fn moving_selection_does_not_add_a_duplicate_detail_row() {
        let mut modal = PrModal::dummy();
        modal.select_prev(); // #801（merged）を選択
        let text = joined(&modal);
        assert_eq!(text.matches("#801").count(), 1);
        assert!(!text.contains("github.com/kkyosuke/usagi/pull/801"));
    }

    #[test]
    fn render_handles_a_missing_title() {
        // タイトル無しの PR は "(no title)" を出す。
        let modal = PrModal::new(vec![pr_entry(7, "https://example.com/pull/7")]);
        let text = joined(&modal);
        assert!(text.contains("#7"));
        assert!(text.contains("(no title)"));
    }

    #[test]
    fn daemon_refresh_metadata_is_rendered_as_safe_status() {
        use usagi_core::domain::pr_inventory::{PrInventory, PrRefreshState, canonicalize};

        let identity = canonicalize("https://github.com/o/r/pull/7").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([identity.clone()]);
        let entries = inventory.entries.values().cloned().collect::<Vec<_>>();
        assert!(joined(&PrModal::new(entries)).contains("refresh pending"));

        inventory.entries.get_mut(&identity).unwrap().refresh = PrRefreshState::BackingOff;
        let entries = inventory.entries.values().cloned().collect::<Vec<_>>();
        assert!(joined(&PrModal::new(entries)).contains("refresh retrying"));
    }

    #[test]
    fn render_shows_an_empty_notice() {
        let text = joined(&PrModal::new(Vec::new()));
        assert!(text.contains("no pull requests"));
    }

    #[test]
    fn render_labels_a_dismissed_pr() {
        let mut pr = pr_entry(3, "https://example.com/pull/3");
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
        assert!(frame[23].starts_with("workspace-row-23-"));
        assert!(text.contains("Pull Request"));
        assert!(text.contains("#812"));
        let modal_row = frame.iter().find(|line| line.contains('┌')).unwrap();
        assert!(modal_row.starts_with('┌'));
        assert!(modal_row.ends_with("┐\u{1b}[0m"));
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

    #[test]
    fn short_terminal_keeps_the_selected_pr_tabs_and_footer_visible() {
        let prs = (1..=10)
            .map(|number| pr_entry(number, &format!("https://example.com/pull/{number}")))
            .collect();
        let mut modal = PrModal::new(prs);
        for _ in 0..8 {
            modal.select_next();
        }
        let base = vec![".".repeat(80); 10];
        let frame = render_over(10, 80, &base, &modal);
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("all"));
        assert!(text.contains("#9"));
        assert!(text.contains("←→: status  ↑↓: select"));
        assert!(text.contains("Enter: open  Esc: close"));
        assert!(!text.contains("#8"));
    }

    #[test]
    fn minimal_body_prioritizes_tabs_and_footers_over_the_pr_list() {
        let lines = body(&PrModal::dummy(), 3)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("all"));
        assert_eq!(lines[1], "  ←→: status  ↑↓: select");
        assert!(lines[2].contains("Enter: open  Esc: close"));
        assert!(lines.iter().all(|line| !line.contains("#812")));
    }
}
