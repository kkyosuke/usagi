use chrono::{DateTime, Utc};
use console::style;

use crate::presentation::tui::widgets;

/// A single entry in the welcome-screen menu.
pub struct MenuItem {
    pub label: &'static str,
    pub key: char,
}

/// A recent workspace entry shown in the welcome screen's right column.
pub struct RecentItem {
    pub label: String,
    pub key: char,
    pub updated_at: DateTime<Utc>,
    pub session_count: usize,
    pub open_issue_count: usize,
    pub pr_count: usize,
}

const TITLE: &str = "USAGI";
const MENU_WIDTH: usize = 18;
const RECENT_WIDTH: usize = 32;
const COLUMN_GAP: usize = 4;
const RECENT_INNER_WIDTH: usize = RECENT_WIDTH - 4;

/// Builds the centred mascot and title block.
///
/// Vertical placement is handled by [`render_frame`], which centres the whole
/// body in the terminal, so this returns no leading padding.
fn header_lines(width: usize) -> Vec<String> {
    widgets::header_lines(width, TITLE, None)
}

/// Builds the left menu column, highlighting the selected entry.
fn menu_column_lines(items: &[MenuItem], selected_index: usize) -> Vec<String> {
    // "> Label..... key" — cursor + 10-char label + right-aligned key.
    let mut lines = vec![String::new()];
    for (i, item) in items.iter().enumerate() {
        let is_selected = i == selected_index;

        let cursor = widgets::cursor_marker(is_selected);
        let label = if is_selected {
            style(format!("{:<10}", item.label))
                .cyan()
                .bold()
                .to_string()
        } else {
            format!("{:<10}", item.label)
        };
        let key = if is_selected {
            style(format!("{:>5}", item.key)).yellow().to_string()
        } else {
            format!("{:>5}", item.key)
        };

        lines.push(format!("{cursor} {label} {key}"));
        lines.push(String::new());
    }
    lines
}

/// Builds one recent-workspace card for the right column.
fn recent_card(item: Option<&RecentItem>, index: usize, now: DateTime<Utc>) -> Vec<String> {
    let fallback_key = char::from_digit((index + 1) as u32, 10).unwrap_or('?');
    match item {
        Some(item) => {
            let title = format!("{} {}", style(item.key).yellow().bold(), item.label);
            let body = vec![style(format!(
                "◷ {}  ⎇ {}  #{}  ● {}",
                widgets::relative_time(item.updated_at, now),
                item.session_count,
                item.pr_count,
                item.open_issue_count
            ))
            .dim()
            .to_string()];
            widgets::boxed(&title, RECENT_INNER_WIDTH, &body)
        }
        None => widgets::boxed(
            &format!("{fallback_key} —"),
            RECENT_INNER_WIDTH,
            &[style("No recent workspace").dim().to_string()],
        ),
    }
}

/// Builds the right-column recent-workspace cards.
///
/// The height is fixed: a heading plus three compact cards. Keeping the section
/// height stable means loading or clearing recents never shifts the mascot row.
fn recent_lines(items: &[RecentItem], now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![String::new(), style("Recent").green().bold().to_string()];
    for i in 0..3 {
        lines.extend(recent_card(items.get(i), i, now));
    }
    lines
}

/// Pads an ANSI-styled segment to a fixed display width, clipping it when
/// necessary so the following column starts in a stable position.
fn pad_segment(segment: &str, width: usize) -> String {
    let segment = widgets::clip_to_width(segment, width);
    let visible = console::measure_text_width(&segment);
    format!("{segment}{}", " ".repeat(width.saturating_sub(visible)))
}

/// Builds the two-column menu block: the existing main menu on the left and up
/// to three recent workspaces on the right.
fn menu_lines(
    width: usize,
    items: &[MenuItem],
    selected_index: usize,
    recent_items: &[RecentItem],
    now: DateTime<Utc>,
) -> Vec<String> {
    let left = menu_column_lines(items, selected_index);
    let right = recent_lines(recent_items, now);
    let row_count = left.len().max(right.len());
    // The divider sits on the same horizontal centre line as the centred mascot
    // and title; the two columns hang off either side of it.
    let divider_col = width / 2;
    let left_col = divider_col.saturating_sub(COLUMN_GAP + MENU_WIDTH);
    let right_col = divider_col + 1 + COLUMN_GAP;
    let gap = " ".repeat(COLUMN_GAP);

    (0..row_count)
        .map(|i| {
            let left = pad_segment(left.get(i).map(String::as_str).unwrap_or(""), MENU_WIDTH);
            let right = pad_segment(right.get(i).map(String::as_str).unwrap_or(""), RECENT_WIDTH);
            let mut line = String::new();
            line.push_str(&" ".repeat(left_col));
            line.push_str(&left);
            let left_visible = left_col + MENU_WIDTH;
            line.push_str(&" ".repeat(divider_col.saturating_sub(left_visible)));
            line.push_str(&style("│").dim().to_string());
            line.push_str(&gap);
            let right_visible = divider_col + 1 + COLUMN_GAP;
            line.push_str(&" ".repeat(right_col.saturating_sub(right_visible)));
            line.push_str(&right);
            widgets::clip_to_width(&line, width)
        })
        .collect()
}

/// Builds the transient notice line under the menu.
///
/// Always returns exactly one line — a blank placeholder when there is no
/// notice — so showing or clearing a notice never shifts the surrounding
/// layout.
fn notice_lines(width: usize, notice: Option<&str>) -> Vec<String> {
    let Some(notice) = notice else {
        return vec![String::new()];
    };
    let padding = " ".repeat(widgets::centered_padding(width, notice.chars().count()));
    vec![format!("{padding}{}", style(notice).yellow())]
}

/// Builds the status footer shown at the bottom of the welcome screen.
///
/// Returns the footer text only; [`render_frame`] pins it to the bottom edge.
fn footer_lines(width: usize) -> Vec<String> {
    // Each menu row shows its shortcut letter (right-aligned); the footer names
    // that those letters select an item directly, so the affordance the rows hint
    // at is spelled out (Enter and the per-row letter both open the entry).
    let footer = format!(
        " v{} | ↑↓: move / Enter or letter: select / 1-3: recent / q: quit",
        env!("CARGO_PKG_VERSION")
    );
    vec![widgets::dim_line(width, &footer)]
}

/// The number of blank rows above the mascot that vertically centre the welcome
/// body over its pinned footer — i.e. the row the mascot's first line sits on.
///
/// [`render_frame`] uses it to place its own body, and the startup splash — which
/// shows the same mascot and title before the menu takes over — reuses it so the
/// mascot and title sit at exactly the rows the welcome screen places them, and
/// never jump when the menu and footer appear (no layout shift). Every section's
/// row count is independent of the width, so the throwaway width passed to the
/// builders here does not affect the result.
pub fn body_top_padding(
    height: usize,
    items: &[MenuItem],
    recent_items: &[RecentItem],
    notice: Option<&str>,
) -> usize {
    let body = header_lines(0).len()
        + menu_lines(0, items, 0, recent_items, Utc::now()).len()
        + notice_lines(0, notice).len();
    let footer = footer_lines(0).len();
    height.saturating_sub(body + footer) / 2
}

/// Builds the full welcome-screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    items: &[MenuItem],
    selected_index: usize,
    recent_items: &[RecentItem],
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    // The body (mascot, title, menu and notice slot) is centred vertically;
    // the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width);
    body.extend(menu_lines(
        width,
        items,
        selected_index,
        recent_items,
        Utc::now(),
    ));
    body.extend(notice_lines(width, notice));
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);

    // Centre the body in the space above the footer (shared with the splash so
    // the mascot and title line up across the two screens).
    let top_padding = body_top_padding(height, items, recent_items, notice);
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);

    // Push the footer down to the bottom row of the frame.
    let bottom_padding = height.saturating_sub(lines.len() + footer.len());
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.extend(footer);

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_items() -> Vec<MenuItem> {
        vec![
            MenuItem {
                label: "Open",
                key: 'o',
            },
            MenuItem {
                label: "Quit",
                key: 'q',
            },
        ]
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample_recents() -> Vec<RecentItem> {
        let now = now();
        vec![
            RecentItem {
                label: "alpha".to_string(),
                key: '1',
                updated_at: now - chrono::Duration::minutes(11),
                session_count: 2,
                open_issue_count: 4,
                pr_count: 1,
            },
            RecentItem {
                label: "beta".to_string(),
                key: '2',
                updated_at: now - chrono::Duration::hours(3),
                session_count: 1,
                open_issue_count: 0,
                pr_count: 0,
            },
        ]
    }

    #[test]
    fn header_lines_have_no_leading_padding() {
        // Vertical placement is handled by render_frame, so the mascot block
        // itself starts immediately with the first rabbit line.
        let lines = header_lines(80);
        assert!(!lines[0].is_empty());
        assert!(lines.iter().any(|l| l.contains("USAGI")));
    }

    #[test]
    fn menu_lines_marks_only_the_selected_entry() {
        let items = sample_items();
        let lines = menu_lines(80, &items, 0, &[], now());
        assert!(lines.iter().any(|l| l.contains("Open")));
        assert!(lines.iter().any(|l| l.contains("Quit")));
        // The selected cursor ">" appears exactly once.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
    }

    #[test]
    fn menu_lines_show_recents_in_the_right_column() {
        let items = sample_items();
        let recents = sample_recents();
        let lines = menu_lines(80, &items, 0, &recents, now());
        let joined = lines.join("\n");
        assert!(joined.contains("Open"));
        assert!(joined.contains("Recent"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains('│'));
        assert!(joined.contains("#1"));
        assert!(joined.contains("11min ago"));
        assert!(joined.contains("1"));
        assert!(joined.contains("2"));
    }

    #[test]
    fn recent_lines_always_reserve_three_numbered_slots() {
        let recents = sample_recents();
        let lines = recent_lines(&recents, now());
        let joined = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        assert!(joined.contains("1 alpha"));
        assert!(joined.contains("2 beta"));
        assert!(joined.contains("3 —"));
        assert!(joined.contains("#1"));
        assert!(joined.contains("● 4"));
        // Heading + three compact card frames.
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn notice_lines_reserve_a_blank_slot_when_absent() {
        // A reserved blank line keeps the layout from shifting when a notice
        // appears or is cleared.
        let lines = notice_lines(80, None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].is_empty());
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let items = sample_items();
        let without = render_frame(24, 80, &items, 0, &[], None);
        let with = render_frame(24, 80, &items, 0, &[], Some("Config is coming soon 🐰"));
        // The frame height is identical whether or not a notice is shown, so
        // the menu and footer never move.
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn notice_lines_renders_text_when_present() {
        let lines = notice_lines(80, Some("hello"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello"));
    }

    #[test]
    fn footer_lines_include_version() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains(env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let items = sample_items();
        let frame = render_frame(0, 0, &items, 1, &sample_recents(), Some("coming soon"));
        let joined = frame.join("\n");
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Open"));
        assert!(joined.contains("Recent"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("coming soon"));
        assert!(joined.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let items = sample_items();
        let height = 40;
        let frame = render_frame(height, 80, &items, 0, &[], None);

        // The frame fills exactly the terminal height...
        assert_eq!(frame.len(), height);
        // ...with the footer on the very last line...
        assert!(frame.last().unwrap().contains(env!("CARGO_PKG_VERSION")));
        // ...and leading blank lines that vertically centre the body.
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
        assert!(frame.iter().any(|l| l.contains("USAGI")));
    }

    #[test]
    fn body_top_padding_is_the_rendered_mascot_row() {
        // The shared placement helper reports exactly the row render_frame puts
        // the mascot on, so the splash can align to it without a layout shift.
        let items = sample_items();
        let height = 40;
        let pad = body_top_padding(height, &items, &[], None);
        let frame = render_frame(height, 80, &items, 0, &[], None);
        assert!(frame[..pad].iter().all(|l| l.is_empty()));
        assert!(console::strip_ansi_codes(&frame[pad]).contains("(\\(\\"));
    }

    #[test]
    fn body_top_padding_is_unaffected_by_the_notice() {
        // The notice slot is always one row whether or not a notice shows, so the
        // mascot's row — and thus the splash's alignment — never depends on it.
        let items = sample_items();
        assert_eq!(
            body_top_padding(40, &items, &[], None),
            body_top_padding(40, &items, &[], Some("Saved 🐰")),
        );
    }

    #[test]
    fn body_top_padding_is_unaffected_by_recent_items() {
        let items = sample_items();
        assert_eq!(
            body_top_padding(40, &items, &[], None),
            body_top_padding(40, &items, &sample_recents(), None),
        );
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let items = sample_items();
        // A terminal too short for the body: no centring padding is added, and
        // the body is never truncated.
        let frame = render_frame(3, 80, &items, 0, &[], None);
        assert!(!frame[0].is_empty());
        assert!(frame.iter().any(|l| l.contains("USAGI")));
        assert!(frame.last().unwrap().contains(env!("CARGO_PKG_VERSION")));
    }
}
