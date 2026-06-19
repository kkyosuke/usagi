//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — the usagi mascot, screen titles, dimmed subtitles/footers, and the modal
//! box that overlays a screen — live here so every screen renders them
//! consistently. Stateful, reusable widgets (e.g. the searchable [`picker`])
//! live in submodules.

pub mod dir_picker;
pub mod picker;
pub mod text_input;

use console::style;

/// The usagi mascot artwork (raw, unstyled lines).
const RABBIT: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

/// Left padding that horizontally centres content of `content_width` columns
/// within a terminal `term_width` columns wide. Saturates to 0 when the content
/// is wider than the terminal.
pub fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// Normalises a raw terminal size, substituting an 80x24 fallback for the
/// zeroes that non-interactive environments report.
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// Centres a single line of `text` by left-padding it with spaces.
fn centered(width: usize, text: &str) -> String {
    let padding = " ".repeat(centered_padding(width, text.chars().count()));
    format!("{padding}{text}")
}

/// The usagi mascot, centred for the terminal width and styled magenta-bold.
///
/// The whole block shares a single padding so the art stays aligned.
pub fn rabbit_lines(width: usize) -> Vec<String> {
    let rabbit_width = RABBIT.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let padding = " ".repeat(centered_padding(width, rabbit_width));
    RABBIT
        .iter()
        .map(|line| {
            style(format!("{padding}{line}"))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// The raw (unstyled) lines of the usagi mascot, for callers that place the art
/// themselves rather than centring it (e.g. the home screen's top-right update
/// notice).
pub fn rabbit_art() -> [&'static str; 3] {
    RABBIT
}

/// Braille spinner frames cycled beside the loading rabbit, one per tick.
const LOADING_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The hopping rabbit's poses as `(ears, body)`. The ears sit centred over the
/// head (the `∩∩` lands on the `ㅅ`), and each "hop" pose shifts the ears *and*
/// the body together by one column so they bounce as a unit without the ears
/// drifting off the head. The blink (`-ㅅ-`) lands on the third pose, so cycling
/// the poses reads as a rabbit hopping in place.
const LOADING_POSES: [(&str, &str); 4] = [
    ("  ∩∩", "(･ㅅ･)づ"),
    ("   ∩∩", " (･ㅅ･)づ"),
    ("  ∩∩", "(-ㅅ-)づ"),
    ("   ∩∩", " (･ㅅ･)づ"),
];

/// A two-line "loading" rabbit for the home screen's top-right corner: a hopping
/// usagi with a braille spinner and a short `label` (e.g. `削除中… 2/5`). `frame`
/// is a monotonically advancing tick — the pose and spinner are picked from it,
/// so painting successive frames animates the rabbit.
///
/// Both rows are padded to a common block width and styled magenta-bold (the
/// mascot's colour), so the block right-aligns cleanly when
/// [`overlay_top_right`](super::super::tui::home::ui) anchors it to the top rows
/// — exactly like the [`update_banner`](super::super::tui::home::ui) notice it
/// shares that corner with.
pub fn loading_rabbit(frame: usize, label: &str) -> Vec<String> {
    let (ears, body) = LOADING_POSES[frame % LOADING_POSES.len()];
    let spinner = LOADING_SPINNER[frame % LOADING_SPINNER.len()];
    let rows = [ears.to_string(), format!("{body}{spinner} {label}")];
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            style(format!("{row}{}", " ".repeat(pad)))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// Faces the time-based loading rabbit ([`loading_rabbit_timed`]) cycles
/// through. Each is a three-cell `XㅅX` mask whose side glyphs are width-1, so
/// the centre `ㅅ` always lands in the same display column and the ears stay
/// over the head no matter which face shows. They convey no progress — the
/// caller advances `face_index` on a wall-clock timer, so the expression simply
/// changes on its own while a background task runs.
const LOADING_FACES: [&str; 6] = ["･ㅅ･", "-ㅅ-", "^ㅅ^", "oㅅo", ">ㅅ<", "=ㅅ="];

/// A two-line loading rabbit whose **bounce and face advance on separate axes**:
/// `hop_frame` drives the hop (and the braille spinner), while `face_index`
/// picks the [`LOADING_FACES`] expression. Used by the background-install
/// overlay, where there is no progress to report — the caller derives both
/// indices from elapsed time, so the rabbit hops and changes expression purely
/// with the clock.
///
/// Like [`loading_rabbit`], both rows are padded to a common block width and
/// styled magenta-bold so the block right-aligns cleanly when
/// [`overlay_top_right`] anchors it to the top-right corner.
pub fn loading_rabbit_timed(hop_frame: usize, face_index: usize, label: &str) -> Vec<String> {
    // The hop shifts the ears and body together by one column, exactly as the
    // progress-driven `loading_rabbit` poses do, so the bounce reads the same.
    let lead = " ".repeat(hop_frame % 2);
    let face = LOADING_FACES[face_index % LOADING_FACES.len()];
    let spinner = LOADING_SPINNER[hop_frame % LOADING_SPINNER.len()];
    let rows = [
        format!("  {lead}∩∩"),
        format!("{lead}({face})づ{spinner} {label}"),
    ];
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            style(format!("{row}{}", " ".repeat(pad)))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// A two-line "finished" rabbit for the background-install overlay: a resting
/// usagi with a happy (`^ㅅ^`) or dejected (`>ㅅ<`) face and the outcome
/// `message`. No spinner — the work is done. Padded and styled like
/// [`loading_rabbit_timed`] so it drops into the same corner.
pub fn done_rabbit(ok: bool, message: &str) -> Vec<String> {
    let face = if ok { "^ㅅ^" } else { ">ㅅ<" };
    let mark = if ok { "✓" } else { "✗" };
    let rows = ["  ∩∩".to_string(), format!("({face})づ{mark} {message}")];
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            style(format!("{row}{}", " ".repeat(pad)))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// Right-anchors each line of `banner` onto the `lines` starting at row `top`,
/// appending it after the existing content. A row is only overlaid when its
/// current content does not reach the banner's left column, so busy rows (a
/// session card, a live terminal) are never clobbered; the banner is skipped
/// entirely when it cannot fit the width.
///
/// Shared by the home screen's top-right notices and by
/// [`FramePainter`](super::screen::FramePainter), which overlays the global
/// background-install rabbit onto whatever screen is showing.
pub fn overlay_top_right(lines: &mut [String], top: usize, width: usize, banner: &[String]) {
    let block_w = banner
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w >= width {
        return;
    }
    let target_left = width - block_w;
    for (offset, segment) in banner.iter().enumerate() {
        let Some(base) = lines.get_mut(top + offset) else {
            break;
        };
        let base_w = console::measure_text_width(base);
        if base_w <= target_left {
            base.push_str(&" ".repeat(target_left - base_w));
            base.push_str(segment);
        }
    }
}

/// A centred, green-bold screen title.
pub fn title_line(width: usize, title: &str) -> String {
    style(centered(width, title)).green().bold().to_string()
}

/// A centred, dimmed line — used for subtitles and footers.
pub fn dim_line(width: usize, text: &str) -> String {
    style(centered(width, text)).dim().to_string()
}

/// A left/right value chooser — the shared rendering primitive for every
/// settings field that cycles through choices.
///
/// The value is always wrapped in chevrons — `< Dark >` — so every field reads
/// as a left/right selector and the chevrons line up in a single column down
/// the screen. Colour conveys state: the `focused` row is bright (cyan-bold),
/// the rest are dimmed.
///
/// `changed` marks a value that differs from what is saved on disk: it is
/// painted yellow (taking priority over the focused/idle colours) so unsaved
/// edits stand out at a glance.
pub fn chooser(value: &str, focused: bool, changed: bool) -> String {
    let paint = |text: &str| {
        let styled = style(text.to_string());
        if changed {
            styled.yellow().bold()
        } else if focused {
            styled.cyan().bold()
        } else {
            styled.dim()
        }
        .to_string()
    };

    format!("{} {} {}", paint("<"), paint(value), paint(">"))
}

/// Wraps `lines` in a single-bordered box `inner_width` columns wide, with
/// `title` embedded in the top border.
///
/// Each content line is padded — by *display* width, so text carrying ANSI
/// styling still aligns — to `inner_width`, with one space of breathing room on
/// each side. The returned rows are not yet placed; [`render_modal`] centres
/// them. A shared primitive so every modal dialog shares one frame.
pub fn boxed(title: &str, inner_width: usize, lines: &[String]) -> Vec<String> {
    // Columns between the two corner glyphs: the content area plus one space of
    // padding on each side.
    let span = inner_width + 2;
    let label = if title.is_empty() {
        String::new()
    } else {
        format!("─ {title} ")
    };
    let label_width = console::measure_text_width(&label);
    let top = format!("┌{label}{}┐", "─".repeat(span.saturating_sub(label_width)));
    let bottom = format!("└{}┘", "─".repeat(span));

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(top);
    for line in lines {
        let pad = inner_width.saturating_sub(console::measure_text_width(line));
        out.push(format!("│ {line}{} │", " ".repeat(pad)));
    }
    out.push(bottom);
    out
}

/// Renders `body` inside a centred [`boxed`] modal for a raw terminal size.
///
/// The box is centred both horizontally and vertically over an otherwise blank
/// frame, mirroring how the full-screen screens build their frames so the event
/// loop can clear and redraw it the same way.
pub fn render_modal(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    let box_lines = boxed(title, inner_width, body);
    // The box is `inner_width` plus the two spaces of padding and two borders.
    let pad = " ".repeat(centered_padding(width, inner_width + 4));

    let mut lines = Vec::with_capacity(height);
    let top_padding = height.saturating_sub(box_lines.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    for line in &box_lines {
        lines.push(format!("{pad}{line}"));
    }
    while lines.len() < height {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_padding_centers_content() {
        assert_eq!(centered_padding(80, 10), 35);
        assert_eq!(centered_padding(81, 10), 35);
    }

    #[test]
    fn centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(5, 10), 0);
    }

    #[test]
    fn normalize_size_substitutes_fallbacks_for_zero() {
        assert_eq!(normalize_size(0, 0), (24, 80));
    }

    #[test]
    fn normalize_size_keeps_nonzero_values() {
        assert_eq!(normalize_size(30, 100), (30, 100));
    }

    #[test]
    fn rabbit_lines_are_three_centered_mascot_rows() {
        let lines = rabbit_lines(80);
        assert_eq!(lines.len(), 3);
        // The mascot face appears, and the block is indented (centred).
        assert!(lines.iter().any(|l| l.contains("(='-')")));
        assert!(lines[0].starts_with(' '));
    }

    #[test]
    fn title_line_contains_the_title() {
        assert!(title_line(80, "USAGI").contains("USAGI"));
    }

    #[test]
    fn loading_rabbit_carries_the_label_and_a_spinner_frame() {
        let lines = loading_rabbit(2, "削除中… 2/5");
        assert_eq!(lines.len(), 2);
        let plain = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        // The label rides the body row, and the blink pose shows on this frame.
        assert!(plain.contains("削除中… 2/5"));
        assert!(plain.contains("(-ㅅ-)"));
        // The braille spinner for frame 2 is present.
        assert!(plain.contains('⠹'));
    }

    #[test]
    fn loading_rabbit_rows_share_one_block_width() {
        // Both rows pad to the widest, so the block right-aligns as a rectangle
        // when anchored to the top-right corner.
        let lines = loading_rabbit(0, "読み込み中…");
        let w0 = console::measure_text_width(&lines[0]);
        let w1 = console::measure_text_width(&lines[1]);
        assert_eq!(w0, w1);
    }

    #[test]
    fn loading_rabbit_animates_across_frames() {
        // Advancing the frame cycles the spinner glyph, so successive paints move.
        let a = console::strip_ansi_codes(&loading_rabbit(0, "x").join("\n")).into_owned();
        let b = console::strip_ansi_codes(&loading_rabbit(1, "x").join("\n")).into_owned();
        assert_ne!(a, b);
    }

    #[test]
    fn loading_rabbit_keeps_the_ears_over_the_head_through_the_hop() {
        // The display column of the first ear must line up with the head centre
        // (`ㅅ`) on both the resting (frame 0) and hopped (frame 1) poses, so the
        // ears never drift off the head as the rabbit bounces.
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for frame in [0usize, 1] {
            let lines = loading_rabbit(frame, "x");
            assert_eq!(
                col_of(&lines[0], '∩'),
                col_of(&lines[1], 'ㅅ'),
                "ears must sit over the head on frame {frame}",
            );
        }
    }

    #[test]
    fn loading_rabbit_timed_carries_the_label_face_and_spinner() {
        let lines = loading_rabbit_timed(0, 0, "LLM 導入中…");
        assert_eq!(lines.len(), 2);
        let plain = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        assert!(plain.contains("LLM 導入中…"));
        // The first face and the frame-0 braille spinner show.
        assert!(plain.contains("(･ㅅ･)"));
        assert!(plain.contains('⠋'));
    }

    #[test]
    fn loading_rabbit_timed_changes_face_with_the_face_index_alone() {
        // The expression advances on its own axis: holding the hop frame fixed
        // and bumping only the face index swaps the face — so the rabbit's mood
        // changes purely on the clock, independent of any progress.
        let a = console::strip_ansi_codes(&loading_rabbit_timed(0, 0, "x").join("\n")).into_owned();
        let b = console::strip_ansi_codes(&loading_rabbit_timed(0, 1, "x").join("\n")).into_owned();
        assert!(a.contains("(･ㅅ･)"));
        assert!(b.contains("(-ㅅ-)"));
    }

    #[test]
    fn loading_rabbit_timed_faces_wrap_and_cover_every_expression() {
        // Indexing wraps modulo the face set, and every face is reachable.
        for (i, face) in LOADING_FACES.iter().enumerate() {
            let plain =
                console::strip_ansi_codes(&loading_rabbit_timed(0, i, "x").join("\n")).into_owned();
            assert!(plain.contains(&format!("({face})")));
        }
        let wrapped = console::strip_ansi_codes(
            &loading_rabbit_timed(0, LOADING_FACES.len(), "x").join("\n"),
        )
        .into_owned();
        assert!(wrapped.contains(&format!("({})", LOADING_FACES[0])));
    }

    #[test]
    fn loading_rabbit_timed_rows_share_one_block_width() {
        let lines = loading_rabbit_timed(1, 2, "導入中…");
        assert_eq!(
            console::measure_text_width(&lines[0]),
            console::measure_text_width(&lines[1]),
        );
    }

    #[test]
    fn loading_rabbit_timed_keeps_the_ears_over_the_head_through_the_hop() {
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for hop in [0usize, 1] {
            let lines = loading_rabbit_timed(hop, 0, "x");
            assert_eq!(
                col_of(&lines[0], '∩'),
                col_of(&lines[1], 'ㅅ'),
                "ears must sit over the head on hop frame {hop}",
            );
        }
    }

    #[test]
    fn done_rabbit_shows_the_outcome_face_and_message() {
        let ok = console::strip_ansi_codes(&done_rabbit(true, "完了").join("\n")).into_owned();
        assert!(ok.contains("(^ㅅ^)"));
        assert!(ok.contains('✓'));
        assert!(ok.contains("完了"));

        let fail = console::strip_ansi_codes(&done_rabbit(false, "失敗").join("\n")).into_owned();
        assert!(fail.contains("(>ㅅ<)"));
        assert!(fail.contains('✗'));
        assert!(fail.contains("失敗"));
    }

    #[test]
    fn done_rabbit_rows_share_one_block_width() {
        let lines = done_rabbit(true, "qwen2.5:7b を導入しました");
        assert_eq!(
            console::measure_text_width(&lines[0]),
            console::measure_text_width(&lines[1]),
        );
    }

    #[test]
    fn overlay_top_right_skips_a_row_whose_content_reaches_the_banner_column() {
        // The first line already fills the width, so the banner cannot be placed
        // on it; a later, empty line still receives its segment.
        let mut lines = vec!["X".repeat(100), String::new()];
        let banner = vec!["AB".to_string(), "CD".to_string()];
        overlay_top_right(&mut lines, 0, 100, &banner);
        assert_eq!(console::measure_text_width(&lines[0]), 100);
        assert!(lines[1].ends_with("CD"));
    }

    #[test]
    fn overlay_top_right_stops_when_the_banner_runs_past_the_last_row() {
        // The banner has more rows than remain from `top`, so placement stops at
        // the end of `lines` instead of panicking.
        let mut lines = vec![String::new()];
        let banner = vec!["AB".to_string(), "CD".to_string(), "EF".to_string()];
        overlay_top_right(&mut lines, 0, 100, &banner);
        assert!(lines[0].ends_with("AB"));
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn overlay_top_right_is_skipped_when_too_narrow_or_empty() {
        // A banner wider than the width is dropped rather than clobbering rows,
        // and an empty banner is a no-op.
        let mut lines = vec![String::new(), String::new()];
        overlay_top_right(&mut lines, 0, 3, &["ABCDE".to_string()]);
        overlay_top_right(&mut lines, 0, 80, &[]);
        assert!(lines.iter().all(|l| l.is_empty()));
    }

    #[test]
    fn dim_line_contains_the_text() {
        assert!(dim_line(80, "hint").contains("hint"));
    }

    #[test]
    fn chooser_always_brackets_the_value() {
        // Chevrons show whether focused or not, so every field reads as a
        // selector and the chevrons align down the column.
        for focused in [true, false] {
            let rendered = chooser("Dark", focused, false);
            assert!(rendered.contains("Dark"));
            assert!(rendered.contains('<'));
            assert!(rendered.contains('>'));
        }
    }

    #[test]
    fn chooser_keeps_the_value_aligned_across_focus() {
        // Focus changes only the colour, not the layout, so the visible width is
        // identical and the column never jumps.
        let focused = console::strip_ansi_codes(&chooser("On", true, false)).into_owned();
        let idle = console::strip_ansi_codes(&chooser("On", false, false)).into_owned();
        assert_eq!(focused, idle);
    }

    #[test]
    fn chooser_marks_changed_values() {
        // A changed value still renders its text; the colour difference is what
        // signals the unsaved edit, and it applies whether focused or not.
        assert!(chooser("Gemini", true, true).contains("Gemini"));
        assert!(chooser("Gemini", false, true).contains("Gemini"));
    }

    #[test]
    fn boxed_frames_the_lines_with_a_title_and_borders() {
        let lines = boxed("Title", 10, &["hi".to_string(), "world".to_string()]);
        // Two content rows plus the top and bottom borders.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains("Title"));
        assert!(lines[0].ends_with('┐'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(lines.last().unwrap().ends_with('┘'));
        // Content rows are bordered and equal width (padded to the inner width).
        assert!(lines[1].contains("hi"));
        assert!(lines[2].contains("world"));
        assert_eq!(
            console::measure_text_width(&lines[1]),
            console::measure_text_width(&lines[2]),
        );
    }

    #[test]
    fn boxed_without_a_title_is_all_dashes_on_top() {
        let lines = boxed("", 4, &["x".to_string()]);
        // No title segment: the top border is corners plus a run of dashes.
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains('─'));
        assert!(!lines[0].contains(' '));
    }

    #[test]
    fn render_modal_centers_a_box_over_a_full_frame() {
        let frame = render_modal(24, 80, "Pick", 20, &["row".to_string()]);
        assert_eq!(frame.len(), 24);
        let joined = frame.join("\n");
        assert!(joined.contains("Pick"));
        assert!(joined.contains("row"));
        // The box is offset from the left edge (horizontally centred).
        let box_row = frame.iter().find(|l| l.contains("Pick")).unwrap();
        assert!(box_row.starts_with(' '));
        // Blank rows above the box (vertically centred).
        assert!(frame[0].is_empty());
    }
}
