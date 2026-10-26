//! Two explicit inbox views, sharing their drawn rows with pointer hit testing.
use crate::usecase::fuzzy::contiguous_match;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

use super::{
    AttentionList, InboxView, Key, OverlayIntent, Role, Row, Style, Summary, WorkspaceDeck,
    WorkspaceSlot, modal, rows, widgets,
};
use crate::presentation::widgets::button::InlineButton;

const MAX_INNER_WIDTH: usize = 110;
const MAX_BODY_HEIGHT: usize = 26;

struct Layout {
    inner: usize,
    height: usize,
    body: Vec<String>,
    tabs: Vec<(InboxView, Range<usize>)>,
    item_hits: Vec<(usize, Range<usize>)>,
    items: Vec<Row>,
    compact: bool,
    footer_rows: usize,
    item_height: usize,
}

fn tabs(
    list: &AttentionList,
    summary: &Summary,
    inner: usize,
) -> (String, Vec<(InboxView, Range<usize>)>) {
    let full = format!(
        "  Action needed ({})    Updates ({})    ",
        summary.action_total(),
        summary.updates
    );
    let actions = if widgets::display_width(&full) > inner {
        "Actions"
    } else {
        "Action needed"
    };
    let mut line = String::from("  ");
    let mut hits = Vec::new();
    for (view, label, role) in [
        (
            InboxView::Actions,
            format!("{actions} ({})", summary.action_total()),
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

fn highlight(text: &str, filter: &str, style: Style) -> String {
    let Some(range) = contiguous_match(text, filter).filter(|range| !range.is_empty()) else {
        return style.paint(text);
    };
    let mut spans = [String::new(), String::new(), String::new()];
    let mut position = 0;
    for grapheme in text.graphemes(true) {
        let end = position + grapheme.chars().count();
        let span = if end <= range.start {
            0
        } else if position >= range.end {
            2
        } else {
            1
        };
        spans[span].push_str(grapheme);
        position = end;
    }
    format!(
        "{}{}{}",
        style.paint(&spans[0]),
        style.bold().reverse().paint(&spans[1]),
        style.paint(&spans[2])
    )
}

fn append_items(layout: &mut Layout, list: &AttentionList, capacity: usize) {
    let available = if layout.compact {
        capacity
    } else {
        capacity.saturating_sub(2)
    };
    let count = (available / layout.item_height).max(1);
    let selected = list.index(&layout.items);
    let (start, end) = modal::list_window(layout.items.len(), selected, count);
    if !layout.compact && start > 0 {
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
        let style = if index == selected {
            role.style().bold()
        } else {
            role.style()
        };
        let action = if layout.item_height == 3 {
            item.action.to_owned()
        } else {
            format!(
                "{}  {}",
                widgets::pad_to_width(item.action, 18),
                item.context
            )
        };
        layout.body.push(format!(
            "  {} {}",
            modal::selection_marker(index == selected),
            widgets::clip_to_width(
                &highlight(&action, &list.filter, style),
                layout.inner.saturating_sub(4)
            )
        ));
        if layout.item_height == 3 {
            layout.body.push(format!(
                "    {}",
                widgets::clip_to_width(
                    &highlight(&item.context, &list.filter, style),
                    layout.inner.saturating_sub(4)
                )
            ));
        }
        let reason_style = if index == selected {
            Style::new()
        } else {
            Style::new().dim()
        };
        layout.body.push(format!(
            "    {}",
            widgets::clip_to_width(
                &highlight(&item.reason, &list.filter, reason_style),
                layout.inner.saturating_sub(4)
            )
        ));
        layout.item_hits.push((index, top..layout.body.len()));
    }
    if !layout.compact && end < layout.items.len() {
        layout.body.push(modal::caption(&format!(
            "↓ {} more",
            layout.items.len() - end
        )));
    }
}

fn append_footer(layout: &mut Layout, list: &AttentionList) {
    layout
        .body
        .resize(layout.height - layout.footer_rows, String::new());
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
    if layout.compact {
        let position = if layout.items.is_empty() {
            String::new()
        } else {
            format!(
                "{}/{} · ",
                list.index(&layout.items) + 1,
                layout.items.len()
            )
        };
        layout.body.push(modal::footer(&format!(
            "{position}{}↑↓ · Tab · Esc",
            if open_label.is_empty() {
                ""
            } else {
                "Enter · "
            }
        )));
    } else {
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
}

fn empty_message(list: &AttentionList, unknown: bool) -> &'static str {
    if !list.filter.is_empty() {
        return "No items match this filter.";
    }
    match list.view {
        InboxView::Actions if unknown => "No confirmed actions. Some statuses are still unknown.",
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
    let compact = body_height < 14 || inner < 70;
    let footer_rows = if compact { 1 } else { 2 };
    let item_height = if inner < 52 { 3 } else { 2 };
    let mut layout = Layout {
        inner,
        height: body_height,
        body: Vec::new(),
        tabs: Vec::new(),
        item_hits: Vec::new(),
        items: rows(slots, &list.filter, list.view),
        compact,
        footer_rows,
        item_height,
    };
    let summary = Summary::from_slots(slots);
    let (tab_line, tab_hits) = tabs(list, &summary, inner);
    let mut header = vec![tab_line];
    if !compact {
        header.push(modal::caption(if list.view == InboxView::Actions {
            "Questions and stopped tasks that need your action in usagi."
        } else if summary.has_unknown() {
            "Progress and status updates."
        } else {
            "Progress updates. No action needed here."
        }));
    }
    header.push(format!(
        "  Filter:{}",
        modal::filter_line(&list.filter, list.filter.len(), None)
    ));
    if !compact {
        header.push(String::new());
    }
    let extra = usize::from(summary.has_unknown()) + usize::from(notice.is_some());
    if body_height < header.len() + footer_rows + item_height + extra
        || inner < widgets::display_width(&header[0])
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
    let header_height = header.len();
    layout.body = header;
    let content_height = body_height - header_height - footer_rows - extra;
    if layout.items.is_empty() {
        layout
            .body
            .push(modal::caption(empty_message(list, summary.has_unknown())));
    } else {
        append_items(&mut layout, list, content_height);
    }
    layout
        .body
        .resize(header_height + content_height, String::new());
    if summary.has_unknown() {
        let message = if compact {
            "Checking status…".to_owned()
        } else {
            format!(
                "Status unknown for {} project(s). Retrying automatically.",
                summary.unknown
            )
        };
        layout.body.push(modal::caption(&message));
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
            slots,
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
    slots: &[WorkspaceSlot],
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
            list.set_view(*view, slots);
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
                            assert!(row < layout.height - layout.footer_rows);
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
        let viewport = modal::body_viewport(18, 80, drawn.inner, drawn.height).unwrap();
        // Explanatory text, filter, and footer cells never open a neighboring item.
        for row in (0..drawn.height)
            .filter(|row| !drawn.item_hits.iter().any(|(_, range)| range.contains(row)))
        {
            let key = Key::Click {
                column: viewport.left.try_into().unwrap(),
                row: (viewport.top + row).try_into().unwrap(),
            };
            assert_eq!(
                handle_input(&mut list, &deck.slots, None, &key, 18, 80),
                OverlayIntent::Stay
            );
            assert_eq!(list, before);
        }
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
    fn clicking_a_view_pins_selection_before_background_items_change() {
        let mut deck = deck();
        let first = item("first", AttentionKind::System);
        let second = item("second", AttentionKind::System);
        update(&mut deck, 0, vec![first.clone(), second.clone()]);
        update(&mut deck, 1, vec![]);
        let mut list = AttentionList::default();
        let drawn = layout(&deck.slots, &list, None, 24, 120);
        let viewport = modal::body_viewport(24, 120, drawn.inner, drawn.height).unwrap();
        let (_, range) = drawn
            .tabs
            .iter()
            .find(|(view, _)| *view == InboxView::Updates)
            .unwrap();
        let key = Key::Click {
            column: (viewport.left + range.start).try_into().unwrap(),
            row: viewport.top.try_into().unwrap(),
        };
        handle_input(&mut list, &deck.slots, None, &key, 24, 120);
        update(&mut deck, 0, vec![second.clone(), first.clone()]);
        assert!(
            matches!(handle_input(&mut list, &deck.slots, None, &Key::Enter, 24, 120), OverlayIntent::AttentionVisit { session, .. } if session == first.session)
        );
        update(&mut deck, 0, vec![second]);
        assert_eq!(
            handle_input(&mut list, &deck.slots, None, &Key::Enter, 24, 120),
            OverlayIntent::Stay
        );
    }

    #[test]
    fn empty_filters_unknown_status_and_feedback_keep_their_own_rows() {
        let mut deck = deck();
        update(
            &mut deck,
            0,
            vec![item("session:failed", AttentionKind::Blocked)],
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
                .any(|line| line.contains("Check stopped task"))
        );
        assert!(
            drawn
                .body
                .iter()
                .any(|line| line.contains("Enter: Open session"))
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
        list.view = InboxView::Actions;
        let tiny = layout(&deck.slots, &list, Some("Target unavailable"), 10, 40);
        assert!(!tiny.items.is_empty());
        assert!(tiny.body.iter().any(|line| line.contains("Resize")));
        assert!(
            tiny.body
                .iter()
                .any(|line| line.contains("Target unavailable"))
        );
        assert!(tiny.item_hits.is_empty());
        assert_eq!(
            handle_input(
                &mut list,
                &deck.slots,
                Some("Target unavailable"),
                &Key::Enter,
                10,
                40,
            ),
            OverlayIntent::Stay
        );
        let defaults = layout(&deck.slots, &list, None, 0, 0);
        assert!(defaults.inner > 0);
    }
    #[test]
    fn compact_inbox_keeps_reasons_position_and_click_targets_without_resize() {
        let deck = populated();
        let list = AttentionList::default();
        for height in [12, 13, 14, 18, 24] {
            for width in [40, 64] {
                let drawn = layout(&deck.slots, &list, None, height, width);
                let frame =
                    render(&deck, &list, height, width, &vec![String::new(); height]).join("\n");
                assert!(!frame.contains("Resize"), "{height}x{width}: {frame}");
                assert!(
                    frame.contains("確認したい内容"),
                    "{height}x{width}: {frame}"
                );
                assert!(frame.contains("1/12"));
                assert!(frame.contains("Enter"));
                assert!(drawn.item_hits.iter().all(|(_, range)| range.len() >= 2));
            }
        }
    }

    #[test]
    fn contiguous_search_highlights_complete_graphemes_without_changing_visible_text() {
        let style = Role::Warning.style();
        for (text, filter, emphasized) in [
            ("PR CI checks failed", "ci", "CI"),
            ("日本語 e\u{301} 👩‍💻", "e", "e\u{301}"),
            ("日本語 👩‍💻", "👩", "👩‍💻"),
        ] {
            let painted = highlight(text, filter, style);
            assert_eq!(widgets::strip_ansi(&painted), text);
            assert_eq!(
                widgets::display_width(&painted),
                widgets::display_width(text)
            );
            assert!(painted.contains(&style.bold().reverse().paint(emphasized)));
        }
        assert_eq!(highlight("normal", "", style), style.paint("normal"));
        assert_eq!(highlight("normal", "missing", style), style.paint("normal"));
    }
}
