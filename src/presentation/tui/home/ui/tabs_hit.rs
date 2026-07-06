//! Tab strip rendering and right-pane tab hit tests.

use crate::presentation::theme::Palette;
use console::style;

use super::super::state::HomeState;
use super::super::terminal::tabs::TabStrip;
use super::clip_to_width;
use super::panes::{active_session_header, switch_preview_header, FOCUS_NEW_TAB_LABEL};

pub(super) fn tab_strip_parts(
    strip: &TabStrip,
    loading: Option<(usize, usize)>,
) -> (String, String) {
    let mut chips = String::new();
    let mut marker = String::new();
    for (i, label) in strip.labels.iter().enumerate() {
        if i > 0 {
            chips.push_str(&" ".repeat(TAB_CHIP_GAP));
            marker.push_str(&" ".repeat(TAB_CHIP_GAP));
        }
        let text = tab_chip_text(i, label);
        // Display width (not char count) so the underline marker stays aligned
        // under a non-ASCII chip label, matching the hit test in
        // [`tab_chip_ranges`], which measures the same chip the same way.
        let width = console::measure_text_width(&text);
        // A background pane still starting shows its chip as a left-to-right
        // loading wave (see [`loading_chip`]) rather than the resting dim / active
        // styling — so the new tab reads as "coming up" right where it will land,
        // without a separate indicator. It never carries the active underline: the
        // previously active pane keeps that while the loading one starts.
        if let Some((_, frame)) = loading.filter(|(idx, _)| *idx == i) {
            chips.push_str(&loading_chip(&text, frame));
            marker.push_str(&" ".repeat(width));
        } else if i == strip.active {
            chips.push_str(&style(&text).reverse().bold().to_string());
            marker.push_str(&style("▔".repeat(width)).accent().bold().to_string());
        } else {
            chips.push_str(&style(&text).dim().to_string());
            marker.push_str(&" ".repeat(width));
        }
    }
    (chips, marker)
}

/// Render one tab chip's text as a left-to-right loading wave: the whole chip is
/// dim with a short accent band sweeping across it, so a background pane that is
/// still starting reads as busy. `frame` advances the band; it sweeps the chip
/// and a short tail past the end, giving a brief all-dim beat before it re-enters
/// from the left — a wave that flows, not a bar that fills then snaps back.
pub(super) fn loading_chip(text: &str, frame: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    // Sweep across the chip plus a few columns of tail, so the band leaves the
    // right edge and re-enters the left with a short gap between passes. The `+ 3`
    // keeps the period positive even for an empty chip (which sweeps to "").
    let period = chars.len() + 3;
    let head = frame % period;
    let mut out = String::new();
    for (i, c) in chars.into_iter().enumerate() {
        let s = c.to_string();
        // A two-column bright band trailing the sweep head.
        if i == head || i + 1 == head {
            out.push_str(&style(s).accent().bold().to_string());
        } else {
            out.push_str(&style(s).dim().to_string());
        }
    }
    out
}

/// The divider drawn between the fixed-width header identity and the tab strip,
/// so the session's identity (name / status / agent) reads as a distinct block
/// from its tabs. It reuses the pane divider glyph ([`SEP`](super::SEP)), dimmed.
pub(super) const HEADER_TAB_DIVIDER: &str = " │ ";

/// Lays the preview `header` (the fixed-width identity from [`preview_header`])
/// and the pane tab strip on a single row: the identity, a dim divider, then the
/// numbered chips, with the active-tab underline marker on the row below
/// re-indented to sit under the chips. Because the identity is a constant width,
/// the divider and the chips land in the same column whichever session is shown,
/// so the row does not jitter as the 切替 cursor moves between sessions. With no
/// `strip` (or an empty one) the identity stands alone on one row. Used by both
/// the 切替 (Switch) preview and 没入 (Attached).
pub(super) fn header_tab_rows(
    header: String,
    strip: Option<&TabStrip>,
    loading: Option<(usize, usize)>,
    width: usize,
) -> Vec<String> {
    let Some(strip) = strip.filter(|s| !s.labels.is_empty()) else {
        return vec![clip_to_width(&header, width)];
    };
    let (chips, marker) = tab_strip_parts(strip, loading);
    let divider = style(HEADER_TAB_DIVIDER).dim().to_string();
    // Push the marker right past the identity and the divider so it lands under
    // the chips on the row above. The identity is a fixed width, so this indent
    // is the same for every session.
    let indent = tab_strip_indent(&header);
    vec![
        clip_to_width(&format!("{header}{divider}{chips}"), width),
        clip_to_width(&format!("{}{marker}", " ".repeat(indent)), width),
    ]
}

/// Gap, in columns, between two chips on the strip's top row (and under it on the
/// marker row), so the chips read as separate tabs without a hard separator glyph.
pub(super) const TAB_CHIP_GAP: usize = 2;

/// One chip's text: a leading space, the 1-based tab number, the pane `label`, and
/// a trailing space — ` N label `. The single recipe both the renderer
/// ([`tab_strip_parts`]) and the hit test ([`tab_chip_ranges`]) build from.
pub(super) fn tab_chip_text(index: usize, label: &str) -> String {
    format!(" {} {label} ", index + 1)
}

/// The column the chips begin at, measured from the right pane's left edge: past
/// the fixed-width identity `header` and the [`HEADER_TAB_DIVIDER`]. Matches the
/// indent [`header_tab_rows`] lays the chips at, so [`tab_chip_ranges`] places
/// them where they are actually drawn.
pub(super) fn tab_strip_indent(header: &str) -> usize {
    console::measure_text_width(header) + HEADER_TAB_DIVIDER.chars().count()
}

/// The column range each tab chip occupies on the strip, measured from the right
/// pane's left edge — the [`tab_strip_indent`], then one [`tab_chip_text`] chip
/// per pane with a [`TAB_CHIP_GAP`] between. Reconstructs the layout
/// [`tab_strip_parts`] / [`header_tab_rows`] draw so a click column can be mapped
/// to the tab under it (see [`attached_tab_at`]).
pub(super) fn tab_chip_ranges(header: &str, strip: &TabStrip) -> Vec<std::ops::Range<usize>> {
    let mut col = tab_strip_indent(header);
    let mut ranges = Vec::with_capacity(strip.labels.len());
    for (i, label) in strip.labels.iter().enumerate() {
        if i > 0 {
            col += TAB_CHIP_GAP;
        }
        let width = console::measure_text_width(&tab_chip_text(i, label));
        ranges.push(col..col + width);
        col += width;
    }
    ranges
}

/// The tab a pointer event at the 0-based screen (`col`, `row`) lands on while
/// 没入 (Attached), including the active tab. Returns `None` for an event off the
/// strip rows, off every chip (the indent, the gaps, past the last chip), or
/// when no tab strip is published.
pub(in crate::presentation::tui::home) fn attached_tab_hit(
    state: &HomeState,
    col: u16,
    row: u16,
    geo: super::TerminalGeometry,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    // The strip's rows are the `TAB_BAR_ROWS` just above the terminal body.
    let strip_top = geo.origin_row.checked_sub(super::TAB_BAR_ROWS as u16)?;
    if row < strip_top || row >= geo.origin_row {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let header = active_session_header(state);
    tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))
}

/// The tab a left click at the 0-based screen (`col`, `row`) lands on while 没入
/// (Attached), or `None` when the click is not on a switchable chip. The strip
/// occupies the [`TAB_BAR_ROWS`](super::TAB_BAR_ROWS) rows at the top of the right
/// pane — the embedded terminal `geo` is pushed down by exactly that — so a click
/// on either of those rows, in a chip's column, hits its tab. Returns `None` for a
/// click off the strip rows, off every chip (the indent, the gaps, past the last
/// chip), or on the already-active tab, so the caller only switches on a real
/// change. Mirrors what [`right_pane_contents`] draws for [`Mode::Attached`].
pub(in crate::presentation::tui::home) fn attached_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    geo: super::TerminalGeometry,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    let target = attached_tab_hit(state, col, row, geo)?;
    // A click on the active tab is a no-op: leave it to the caller's selection
    // handling rather than re-driving the same pane.
    (target != strip.active).then_some(target)
}

/// The live-pane tab (0-based, matching [`TabStrip::labels`]) a left click at the
/// 0-based screen (`col`, `row`) lands on while 在席 (Focus), or `None` when the
/// click is not on a changeable pane tab.
///
/// 在席 draws the same two-row header/tab block as 没入 at the top of the right
/// pane, but the terminal body is only a preview and the selector may also sit on
/// the trailing `+ new` tab. The `+ new` chip is only rendered while it is the
/// selected tab, so a click can never land on it (clicking the active tab is a
/// no-op); only the live pane chips are selectable here. This hit-test
/// reconstructs that rendered strip so the event loop can make right-pane pane
/// tabs mouse-selectable, mirroring the keyboard `Ctrl-N` / `Ctrl-P` path.
pub(in crate::presentation::tui::home) fn focus_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?.clone();
    if strip.labels.is_empty() {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let mut labels = strip.labels.clone();
    let active = if state.focus_on_new_tab() {
        labels.push(FOCUS_NEW_TAB_LABEL.to_string());
        labels.len().saturating_sub(1)
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let target = tab_chip_ranges(&header, &combined)
        .into_iter()
        .position(|range| range.contains(&rel_col))?;
    // Clicking the active tab — including the appended `+ new` chip, which only
    // shows while selected — is a no-op; every other hit is a live pane chip.
    (target != combined.active).then_some(target)
}

pub(in crate::presentation::tui::home) fn focus_tab_hit(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    focus_tab_hit_inner(state, col, row, raw_height, raw_width)
}

pub(super) fn focus_tab_hit_inner(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?.clone();
    if strip.labels.is_empty() {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let mut labels = strip.labels.clone();
    let active = if state.focus_on_new_tab() {
        labels.push(FOCUS_NEW_TAB_LABEL.to_string());
        labels.len().saturating_sub(1)
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    tab_chip_ranges(&header, &combined)
        .into_iter()
        .position(|range| range.contains(&rel_col))
        .filter(|target| *target < strip.labels.len())
}

/// The live-pane tab (0-based, matching [`TabStrip::labels`]) a left click at the
/// 0-based screen (`col`, `row`) lands on while 切替 (Switch), or `None` when the
/// click is not on a changeable pane tab.
///
/// 切替's right pane draws the highlighted session's preview and exposes the
/// same tab strip that `←`/`→` navigate by keyboard. This mirrors the renderer's
/// header/geometry so a click on an inactive chip moves the preview — and the
/// pane that `Enter` re-attaches — to that tab without entering 在席 first.
pub(in crate::presentation::tui::home) fn switch_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    if strip.labels.is_empty() {
        return None;
    }
    let (header, live) = switch_preview_header(state);
    if !live {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let target = tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))?;
    // Clicking the active tab is a no-op; inactive chips select that pane.
    (target != strip.active).then_some(target)
}

pub(in crate::presentation::tui::home) fn switch_tab_hit(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    if strip.labels.is_empty() {
        return None;
    }
    let (header, live) = switch_preview_header(state);
    if !live {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))
}
