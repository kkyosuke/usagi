//! Two explicit inbox views, sharing their drawn rows with pointer hit testing.
use std::ops::Range;

use super::{
    AttentionList, InboxView, Key, OverlayIntent, Role, Row, Style, Summary, WorkspaceDeck,
    WorkspaceSlot, modal, rows, widgets,
};
use crate::presentation::widgets::button::InlineButton;

const MAX_INNER_WIDTH: usize = 110;
const MAX_BODY_HEIGHT: usize = 26;
const HEADER_ROWS: usize = 4;
const FOOTER_ROWS: usize = 2;

struct Layout {
    inner: usize,
    height: usize,
    body: Vec<String>,
    tabs: Vec<(InboxView, Range<usize>)>,
    item_hits: Vec<(usize, Range<usize>)>,
    items: Vec<Row>,
}

fn tabs(list: &AttentionList, summary: &Summary) -> (String, Vec<(InboxView, Range<usize>)>) {
    let mut line = String::from("  ");
    let mut hits = Vec::new();
    for (view, label, role) in [
        (
            InboxView::Actions,
            format!("Action needed ({})", summary.action_total()),
            if summary.actions > 0 {
                Role::Warning
            } else {
                Role::Accent
            },
        ),
        (
            InboxView::Updates,
            format!("Updates ({})", summary.updates),
            Role::Info,
        ),
    ] {
        let button = InlineButton::new(&label);
        let start = widgets::display_width(&line);
        let style = if view == list.view {
            role.style().bold().reverse()
        } else if view == InboxView::Actions && summary.actions > 0 {
            role.style()
        } else {
            Style::new().dim()
        };
        line.push_str(&button.render(button.width(), style).line);
        hits.push((view, start..start + button.width()));
        line.push_str("  ");
    }
    (line, hits)
}

fn item_window(
    len: usize,
    selected: usize,
    capacity: usize,
    item_height: usize,
) -> (usize, usize, bool) {
    let mut count = (capacity / item_height).max(1);
    loop {
        let (start, end) = modal::list_window(len, selected, count);
        let extra = usize::from(start > 0) + usize::from(end < len);
        if (end - start) * item_height + extra <= capacity {
            return (start, end, true);
        }
        if count == 1 {
            return (start, end, false);
        }
        count -= 1;
    }
}

fn append_items(layout: &mut Layout, list: &AttentionList, capacity: usize) {
    let item_height = if capacity >= 2 { 2 } else { 1 };
    let selected = list.index(&layout.items);
    let (start, end, indicators) = item_window(layout.items.len(), selected, capacity, item_height);
    if indicators && start > 0 {
        layout.body.push(modal::caption(&format!("↑ {start} more")));
    }
    let role = if list.view == InboxView::Actions {
        Role::Warning
    } else {
        Role::Info
    };
    for index in start..end {
        let item = &layout.items[index];
        let top = layout.body.len();
        let label = format!(
            "{}  {}",
            widgets::pad_to_width(item.action, 18),
            item.context
        );
        let style = if index == selected {
            role.style().bold()
        } else {
            role.style()
        };
        layout.body.push(format!(
            "  {} {}",
            modal::selection_marker(index == selected),
            style.paint(&widgets::clip_to_width(
                &label,
                layout.inner.saturating_sub(4)
            ))
        ));
        if item_height == 2 {
            let reason_style = if index == selected {
                Style::new()
            } else {
                Style::new().dim()
            };
            layout.body.push(reason_style.paint(&widgets::clip_to_width(
                &format!("    {}", item.reason),
                layout.inner,
            )));
        }
        layout.item_hits.push((index, top..layout.body.len()));
    }
    if indicators && end < layout.items.len() {
        layout.body.push(modal::caption(&format!(
            "↓ {} more",
            layout.items.len() - end
        )));
    }
}

fn append_footer(layout: &mut Layout, list: &AttentionList) {
    layout
        .body
        .resize(layout.height - FOOTER_ROWS, String::new());
    let open_label = layout
        .items
        .get(list.index(&layout.items))
        .filter(|item| {
            !layout.item_hits.is_empty()
                && list
                    .selected
                    .as_ref()
                    .is_none_or(|selected| *selected == item.identity)
        })
        .map_or("", |item| item.open_label);
    layout.body.push(modal::footer(&format!(
        "{}↑↓ select · PgUp/PgDn page",
        if open_label.is_empty() {
            String::new()
        } else {
            format!("Enter: {open_label} · click to open · ")
        }
    )));
    layout.body.push(modal::footer(
        "Tab / ←→ switch view · type to filter · Esc close",
    ));
}

fn empty_message(list: &AttentionList, unknown: usize) -> &'static str {
    if !list.filter.is_empty() {
        return "No items match this filter.";
    }
    match list.view {
        InboxView::Actions if unknown > 0 => {
            "No confirmed actions. Some project statuses are still unknown."
        }
        InboxView::Actions => "No action needed. Ongoing work is in Updates.",
        InboxView::Updates => "No progress updates to show.",
    }
}

fn layout(
    slots: &[WorkspaceSlot],
    list: &AttentionList,
    notice: Option<&str>,
    height: usize,
    width: usize,
) -> Layout {
    let (height, width) = widgets::normalize_size(height, width);
    let inner = modal::modal_inner_width(width, MAX_INNER_WIDTH);
    let body_height = modal::reserved_body_height(height, width, MAX_BODY_HEIGHT);
    let mut layout = Layout {
        inner,
        height: body_height,
        body: Vec::new(),
        tabs: Vec::new(),
        item_hits: Vec::new(),
        items: rows(slots, &list.filter, list.view),
    };
    let summary = Summary::from_slots(slots);
    let (tab_line, tab_hits) = tabs(list, &summary);
    if body_height
        < HEADER_ROWS
            + FOOTER_ROWS
            + 1
            + usize::from(summary.unknown > 0)
            + usize::from(notice.is_some())
        || inner < widgets::display_width(&tab_line)
    {
        layout
            .body
            .push(modal::caption("Resize to see actions and updates."));
        if let Some(notice) = notice {
            layout.body.push(modal::error_line(notice, inner));
        }
        return layout;
    }
    layout.tabs = tab_hits;
    layout.body = vec![
        tab_line,
        modal::caption(if list.view == InboxView::Actions {
            "Only items that need your action appear here."
        } else {
            "Progress updates. No action needed here."
        }),
        format!(
            "  Filter:{}",
            modal::filter_line(&list.filter, list.filter.len(), None)
        ),
        String::new(),
    ];
    let content_height = body_height.saturating_sub(
        HEADER_ROWS
            + FOOTER_ROWS
            + usize::from(summary.unknown > 0)
            + usize::from(notice.is_some()),
    );
    if layout.items.is_empty() {
        layout
            .body
            .push(modal::caption(empty_message(list, summary.unknown)));
    } else {
        append_items(&mut layout, list, content_height);
    }
    layout
        .body
        .resize(HEADER_ROWS + content_height, String::new());
    if summary.unknown > 0 {
        layout.body.push(modal::caption(&format!(
            "Status unknown for {} project(s). Retrying automatically.",
            summary.unknown
        )));
    }
    if let Some(notice) = notice {
        layout.body.push(modal::error_line(notice, inner));
    }
    append_footer(&mut layout, list);
    layout
}

pub(in crate::presentation::workspace_deck) fn render(
    deck: &WorkspaceDeck,
    list: &AttentionList,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let layout = layout(&deck.slots, list, deck.notice(), height, width);
    modal::render_body_over(
        height,
        width,
        base,
        "All projects · Inbox",
        layout.inner,
        layout.height,
        layout.body,
    )
}

pub(in crate::presentation::workspace_deck) fn handle_input(
    list: &mut AttentionList,
    slots: &[WorkspaceSlot],
    notice: Option<&str>,
    key: &Key,
    height: usize,
    width: usize,
) -> OverlayIntent {
    if matches!(key, Key::PageUp | Key::PageDown) {
        let layout = layout(slots, list, notice, height, width);
        let page = layout.item_hits.len();
        if page > 0 {
            let selected = list.index(&layout.items);
            let next = if *key == Key::PageUp {
                selected.saturating_sub(page)
            } else {
                (selected + page).min(layout.items.len() - 1)
            };
            list.selected = Some(layout.items[next].identity.clone());
        }
        return OverlayIntent::Stay;
    }
    if let Key::Click { column, row } = key {
        return click(
            list,
            &layout(slots, list, notice, height, width),
            *column,
            *row,
            height,
            width,
        );
    }
    if *key == Key::Enter
        && layout(slots, list, notice, height, width)
            .item_hits
            .is_empty()
    {
        return OverlayIntent::Stay;
    }
    list.handle(key, slots)
}

fn click(
    list: &mut AttentionList,
    layout: &Layout,
    column: u16,
    row: u16,
    height: usize,
    width: usize,
) -> OverlayIntent {
    let Some(viewport) = modal::body_viewport(height, width, layout.inner, layout.height) else {
        return OverlayIntent::Stay;
    };
    let column = usize::from(column);
    let row = usize::from(row);
    if column < viewport.left || column >= viewport.left + viewport.cols || row < viewport.top {
        return OverlayIntent::Stay;
    }
    let column = column - viewport.left;
    let row = row - viewport.top;
    if row == 0 {
        if let Some((view, _)) = layout
            .tabs
            .iter()
            .find(|(_, range)| range.contains(&column))
        {
            list.set_view(*view);
        }
    } else if let Some((index, _)) = layout
        .item_hits
        .iter()
        .find(|(_, range)| range.contains(&row))
    {
        return layout.items[*index].intent.clone();
    }
    OverlayIntent::Stay
}

#[cfg(test)]
mod tests {
    use super::super::tests::{deck, item, update};
    use super::*;
    use usagi_core::domain::attention::AttentionKind;

    fn populated() -> WorkspaceDeck {
        let mut deck = deck();
        let entries = (0..12)
            .map(|index| {
                let mut entry = item(&format!("question-{index}"), AttentionKind::Decision);
                entry.label = format!("日本語の作業 {index} 👩‍💻");
                entry.reason = "確認したい内容をここに表示します".repeat(8);
                entry
            })
            .collect();
        update(&mut deck, 0, entries);
        update(&mut deck, 1, vec![]);
        deck
    }

    #[test]
    fn rendered_tabs_and_rows_share_bounded_click_geometry() {
        let deck = populated();
        for height in [5, 12, 13, 14, 18, 24, 40] {
            for width in [2, 4, 20, 40, 64, 80, 140] {
                for selected in [0, 6, 11] {
                    let mut list = AttentionList {
                        selected: Some(
                            rows(&deck.slots, "", InboxView::Actions)[selected]
                                .identity
                                .clone(),
                        ),
                        ..AttentionList::default()
                    };
                    let layout = layout(&deck.slots, &list, None, height, width);
                    let frame = render(&deck, &list, height, width, &vec![String::new(); height]);
                    assert_eq!(frame.len(), height);
                    assert!(
                        frame
                            .iter()
                            .all(|line| widgets::display_width(line) <= width)
                    );
                    if layout.item_hits.is_empty() {
                        assert_eq!(
                            handle_input(&mut list, &deck.slots, None, &Key::Enter, height, width),
                            OverlayIntent::Stay
                        );
                        continue;
                    }
                    assert!(layout.item_hits.iter().any(|(index, _)| *index == selected));
                    let viewport =
                        modal::body_viewport(height, width, layout.inner, layout.height).unwrap();
                    for (view, range) in &layout.tabs {
                        for column in [range.start, range.end - 1] {
                            let mut clicked = list.clone();
                            let key = Key::Click {
                                column: (viewport.left + column).try_into().unwrap(),
                                row: viewport.top.try_into().unwrap(),
                            };
                            assert_eq!(
                                handle_input(&mut clicked, &deck.slots, None, &key, height, width),
                                OverlayIntent::Stay
                            );
                            assert_eq!(clicked.view, *view);
                        }
                    }
                    for (index, range) in &layout.item_hits {
                        for row in range.clone() {
                            assert!(row < layout.height - FOOTER_ROWS);
                            let key = Key::Click {
                                column: (viewport.left + 2).try_into().unwrap(),
                                row: (viewport.top + row).try_into().unwrap(),
                            };
                            assert_eq!(
                                handle_input(
                                    &mut list.clone(),
                                    &deck.slots,
                                    None,
                                    &key,
                                    height,
                                    width
                                ),
                                layout.items[*index].intent
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paging_uses_the_visible_item_count_and_ignores_noninteractive_cells() {
        let deck = populated();
        let mut list = AttentionList::default();
        let drawn = layout(&deck.slots, &list, None, 18, 80);
        let page = drawn.item_hits.len();
        assert!(page > 0 && page < 8);
        handle_input(&mut list, &deck.slots, None, &Key::PageDown, 18, 80);
        assert_eq!(list.index(&drawn.items), page);
        handle_input(&mut list, &deck.slots, None, &Key::PageUp, 18, 80);
        assert_eq!(list.index(&drawn.items), 0);
        let before = list.clone();
        for key in [
            Key::Click { column: 0, row: 0 },
            Key::Click {
                column: 139,
                row: 0,
            },
            Key::Click {
                column: 10,
                row: 39,
            },
        ] {
            assert_eq!(
                handle_input(&mut list, &deck.slots, None, &key, 40, 140),
                OverlayIntent::Stay
            );
            assert_eq!(list, before);
        }
        assert_eq!(
            handle_input(
                &mut list,
                &deck.slots,
                None,
                &Key::Click { column: 0, row: 0 },
                5,
                2
            ),
            OverlayIntent::Stay
        );
        assert_eq!(
            handle_input(&mut list, &deck.slots, None, &Key::PageDown, 5, 2),
            OverlayIntent::Stay
        );
        handle_input(&mut list, &deck.slots, None, &Key::Down, 18, 80);
        assert_eq!(list.index(&drawn.items), 1);
    }

    #[test]
    fn empty_filters_unknown_status_and_feedback_keep_their_own_rows() {
        let mut deck = deck();
        update(
            &mut deck,
            0,
            vec![item("pr:failed", AttentionKind::Blocked)],
        );
        let mut list = AttentionList::default();
        let drawn = layout(&deck.slots, &list, Some("Target unavailable"), 24, 80);
        assert!(
            drawn
                .body
                .iter()
                .any(|line| line.contains("Status unknown"))
        );
        assert!(
            drawn
                .body
                .iter()
                .any(|line| line.contains("Target unavailable"))
        );
        assert!(
            drawn
                .body
                .iter()
                .any(|line| line.contains("Check PR problem"))
        );
        assert!(
            drawn
                .body
                .iter()
                .any(|line| line.contains("Enter: Open PR"))
        );
        list.filter = "no match".into();
        assert!(
            layout(&deck.slots, &list, None, 24, 80)
                .body
                .iter()
                .any(|line| line.contains("No items match"))
        );
        list.filter.clear();
        list.view = InboxView::Updates;
        assert!(
            layout(&deck.slots, &list, None, 24, 80)
                .body
                .iter()
                .any(|line| line.contains("No progress updates"))
        );
        assert!(
            layout(&deck.slots, &list, Some("Safe error"), 13, 80)
                .body
                .iter()
                .any(|line| line.contains("Safe error"))
        );
        let defaults = layout(&deck.slots, &list, None, 0, 0);
        assert!(defaults.inner > 0);
    }
}
