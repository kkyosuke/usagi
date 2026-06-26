//! The usagi mascot artwork and its animated renderers.
//!
//! These are presentation *assets* — the static mascot, the farewell box, and
//! the loading / running / multiplying rabbits the screens animate — rather than
//! general layout primitives. They live apart from [`super`]'s
//! layout/box/colour helpers so the shared widget module stays a thin toolkit
//! and the演出 (mascot animation) sits in one place. Every renderer here builds
//! on [`super`]'s primitives ([`centered_padding`](super::centered_padding),
//! [`spinner_char`](super::spinner_char)) so the art stays consistent with the
//! rest of the TUI. The functions are re-exported from [`super`], so callers
//! still reach them as `widgets::rabbit_lines` etc.

use console::{style, Style};

/// The usagi mascot artwork (raw, unstyled lines).
const RABBIT: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

/// The display width of the mascot — the widest of its [`RABBIT`] rows. The
/// shared measure [`rabbit_lines`] centres by, and the open→home transition
/// lifts the rabbit off from.
pub fn rabbit_width() -> usize {
    RABBIT.iter().map(|l| l.chars().count()).max().unwrap_or(0)
}

/// The number of rows the mascot art spans, so a caller can reserve (or blank)
/// exactly the rows the rabbit occupies.
pub fn rabbit_height() -> usize {
    RABBIT.len()
}

/// The usagi mascot's lines indented to begin at column `col`, styled
/// magenta-bold. The whole block shares the one indent so the art stays aligned.
///
/// The shared placement primitive: [`rabbit_lines`] centres the art with it,
/// while the open→home transition glides the same art across the screen by
/// advancing `col` (and the row it is drawn at) frame by frame.
pub fn rabbit_lines_at(col: usize) -> Vec<String> {
    let padding = " ".repeat(col);
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

/// The usagi mascot, centred for the terminal width and styled magenta-bold.
pub fn rabbit_lines(width: usize) -> Vec<String> {
    rabbit_lines_at(super::centered_padding(width, rabbit_width()))
}

/// The mascot waving goodbye, drawn inside the farewell box: the usagi from
/// [`RABBIT`] with a raised paw (`ﾉ`) and its parting words alongside.
const FAREWELL_ART: [&str; 3] = ["  (\\(\\", " ( ^ω^)ﾉ  またね、ぴょん！", " o(_(\")(\")"];
/// Spaces padding the art from the box's side borders.
const FAREWELL_PAD: usize = 2;

/// The rounded box bidding the user farewell — shown both when usagi tears down
/// the alternate screen on exit and when the `quit`/`exit` command runs, so the
/// two share one look.
///
/// The box is sized to the widest art row ([`console::measure_text_width`],
/// matching how the rest of the TUI counts columns) and every row is padded to
/// that width, so the right edge lines up despite the art's mix of half- and
/// full-width characters. The frame is dim and the rabbit cyan — a soft pairing
/// that echoes the TUI's accent palette without shouting. The embedded ANSI
/// survives both the raw exit write and the log pane's pass-through rendering of
/// `Output` lines.
pub fn farewell_lines() -> Vec<String> {
    let content = FAREWELL_ART
        .iter()
        .map(|l| console::measure_text_width(l))
        .max()
        .unwrap_or(0);
    let inner = content + FAREWELL_PAD * 2;
    let rule = "─".repeat(inner);
    let frame = Style::new().dim();
    let rabbit = Style::new().cyan();

    let mut lines = Vec::with_capacity(FAREWELL_ART.len() + 2);
    lines.push(frame.apply_to(format!("╭{rule}╮")).to_string());
    for art in FAREWELL_ART {
        let right = inner - FAREWELL_PAD - console::measure_text_width(art);
        lines.push(format!(
            "{}{}{}",
            frame.apply_to(format!("│{}", " ".repeat(FAREWELL_PAD))),
            rabbit.apply_to(art),
            frame.apply_to(format!("{}│", " ".repeat(right))),
        ));
    }
    lines.push(frame.apply_to(format!("╰{rule}╯")).to_string());
    lines
}

/// The mascot's mood while it rests at the bottom of the workspace sidebar — one
/// per home-screen engagement mode (切替 / 在席 / 没入), so the resting rabbit's
/// expression and gesture mirror what the user is doing. The presentation layer
/// maps its [`Mode`](crate::presentation::tui::home) onto this so the widget
/// stays decoupled from the screen's own enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RabbitMood {
    /// 切替 (Switch): browsing the session list — ears up, looking around (`?`).
    Browsing,
    /// 在席 (Focus): a session is in hand — bright-eyed and attentive, a raised
    /// paw (`/`).
    Attentive,
    /// 没入 (Attached): immersed in the live terminal — eyes screwed up in
    /// concentration, paws at work (`9`).
    Working,
}

impl RabbitMood {
    /// The mood's face row (the mascot's middle line) and the colour the whole
    /// block is painted: magenta while browsing (the mascot's resting colour),
    /// cyan while attending a session (the accent the right pane uses), and green
    /// while a turn runs (matching the `▶ running` accent).
    fn face_and_style(self) -> (&'static str, Style) {
        match self {
            RabbitMood::Browsing => (" (o.o)? ", Style::new().magenta().bold()),
            RabbitMood::Attentive => (" (^.^)/ ", Style::new().cyan().bold()),
            RabbitMood::Working => (" (>.<)9 ", Style::new().green().bold()),
        }
    }
}

/// The mood mascot's three raw rows — ears, face, feet — aligned so the ears, the
/// head, and the body share one left edge. Shared by [`workspace_rabbit`] and
/// [`workspace_rabbit_speaking`] so the resting and speaking rabbits stay identical.
///
/// The shared [`RABBIT`] ears and feet are tuned for the 6-wide resting face
/// (`(='-')`): the ears carry two leading spaces to centre over it and the feet
/// one. A mood face is a 5-wide head (`(>.<)`) plus a side paw (`?` / `/` / `9`),
/// so the head sits one column to the left. To keep the rabbit coherent the ears
/// drop one leading space (landing over the head rather than leaning onto the paw)
/// and the feet drop theirs too, so the body's `(` lines up under the head's and
/// the ears' `(` instead of trailing one column to the right.
fn mood_mascot_rows(face: &str) -> [String; 3] {
    [
        format!(" {}", RABBIT[0].trim()),
        face.to_string(),
        RABBIT[2].trim_start().to_string(),
    ]
}

/// The resting mascot for the bottom of the workspace sidebar, its face and
/// gesture chosen by `mood` so the rabbit reflects the current engagement mode.
/// Only the middle face row changes between moods, so the usagi stays recognisably
/// the same animal while its expression shifts; the ears/head/body alignment is
/// [`mood_mascot_rows`]'. Every row is padded to a common block width and painted
/// the mood's colour, so the block tiles as a rectangle wherever it is placed.
pub fn workspace_rabbit(mood: RabbitMood) -> Vec<String> {
    let (face, paint) = mood.face_and_style();
    let rows = mood_mascot_rows(face);
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            paint
                .apply_to(format!("{row}{}", " ".repeat(pad)))
                .to_string()
        })
        .collect()
}

/// Columns the speech bubble spends on chrome around its text: a rounded border
/// and one space of padding on each side.
const SPEECH_CHROME: usize = 4;

/// The resting workspace mascot *speaking* `speech`: a yellow speech bubble
/// carrying those lines sits above the [`workspace_rabbit`], with a tail pointing
/// down to the usagi's head — so the rabbit reads as saying them. This is where
/// the home screen surfaces the "update available" notice (the message and the
/// new version), moved off the top-right corner so the news comes from the
/// mascot.
///
/// `speech`'s lines are wrapped to fit `max_width` (the sidebar width), so a long
/// message flows onto more bubble rows rather than overrunning the pane; the
/// bubble is painted the update accent (yellow-bold) while the rabbit keeps its
/// `mood` colour, separating the alert from the mascot. When `max_width` leaves
/// no room for even a one-column bubble, or `speech` is empty, it falls back to
/// the silent [`workspace_rabbit`]. Every row is padded to a common block width
/// so the block tiles as a rectangle wherever it is placed.
pub fn workspace_rabbit_speaking(
    mood: RabbitMood,
    speech: &[String],
    max_width: usize,
) -> Vec<String> {
    let inner = max_width.saturating_sub(SPEECH_CHROME);
    // Wrap every speech line to the bubble's inner width, flattening to the
    // bubble's content rows.
    let content: Vec<String> = speech
        .iter()
        .flat_map(|line| super::wrap_to_width(line, inner))
        .collect();
    if content.is_empty() {
        // Too narrow for a bubble (or nothing to say): rest silently instead.
        return workspace_rabbit(mood);
    }
    let content_w = content
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);

    // The bubble is the update accent (yellow-bold); the rabbit keeps its mood
    // colour, so the alert and the mascot stay visually distinct.
    let bubble = Style::new().yellow().bold();
    // Columns between the corner glyphs: the content area plus a padding space on
    // each side.
    let span = content_w + 2;

    let mut rows: Vec<String> = Vec::with_capacity(content.len() + 1 + RABBIT.len());
    rows.push(
        bubble
            .apply_to(format!("╭{}╮", "─".repeat(span)))
            .to_string(),
    );
    for line in &content {
        let pad = content_w.saturating_sub(console::measure_text_width(line));
        rows.push(
            bubble
                .apply_to(format!("│ {line}{} │", " ".repeat(pad)))
                .to_string(),
        );
    }
    // The bottom border carries the speech tail (`┬`) over the mascot's head:
    // the face's nose sits at column 3 (a leading space, then the head's `(>.<)`),
    // so the bubble reads as coming from the usagi just below.
    let mut bottom = String::from("╰");
    for i in 0..span {
        bottom.push(if i == 2 { '┬' } else { '─' });
    }
    bottom.push('╯');
    rows.push(bubble.apply_to(bottom).to_string());

    // The resting mascot below, in its mood colour — only the face row changes.
    let (face, paint) = mood.face_and_style();
    for art in mood_mascot_rows(face) {
        rows.push(paint.apply_to(art).to_string());
    }

    // Pad every row to the widest so the block tiles as a rectangle.
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            format!("{row}{}", " ".repeat(pad))
        })
        .collect()
}

/// The display width of the [`workspace_rabbit`] block, so a caller can check the
/// sidebar is wide enough to hold it before placing it (and skip it otherwise,
/// rather than overrunning a narrow pane).
pub fn workspace_rabbit_width() -> usize {
    // Every mood's face is the same width, so any one stands in for the block.
    mood_mascot_rows(RabbitMood::Working.face_and_style().0)
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0)
}

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
/// [`overlay_top_right`](super::overlay_top_right) anchors it to the top rows
/// — exactly like the [`update_banner`](super::super::home::ui) notice it
/// shares that corner with.
pub fn loading_rabbit(frame: usize, label: &str) -> Vec<String> {
    let (ears, body) = LOADING_POSES[frame % LOADING_POSES.len()];
    let spinner = super::spinner_char(frame);
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
/// [`overlay_top_right`](super::overlay_top_right) anchors it to the top-right
/// corner.
pub fn loading_rabbit_timed(hop_frame: usize, face_index: usize, label: &str) -> Vec<String> {
    // The hop shifts the ears and body together by one column, exactly as the
    // progress-driven `loading_rabbit` poses do, so the bounce reads the same.
    let lead = " ".repeat(hop_frame % 2);
    let face = LOADING_FACES[face_index % LOADING_FACES.len()];
    let spinner = super::spinner_char(hop_frame);
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

/// The running usagi's two content rows — `(ears, body)` — by travel direction.
/// Speed lines (`ﾐ`) trail *behind* the run — on the left when heading right, on
/// the right when heading left — so the rabbit reads as dashing that way while
/// the face keeps its single `ㅅ` nose. The head's `ㅅ` is width-2 like every
/// other usagi face, and the ears sit centred over it (each direction pads the
/// ears so they stay above the head). [`running_rabbit`] draws these as a
/// three-row block that bobs up and down so a rabbit translated across the
/// screen reads as bounding.
const RUNNER_RIGHT: [&str; 2] = ["   ∩∩", "ﾐ(･ㅅ･)"];
const RUNNER_LEFT: [&str; 2] = ["  ∩∩", "(･ㅅ･)ﾐ"];

/// The display width of the running usagi sprite, so a caller can bound the
/// rabbit's horizontal travel against the terminal width (the rightmost column
/// it may start at is `width - running_rabbit_width()`).
pub fn running_rabbit_width() -> usize {
    RUNNER_RIGHT
        .iter()
        .chain(RUNNER_LEFT.iter())
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0)
}

/// A three-row running usagi at horizontal offset `col`, facing right
/// (`face_right`) or left, drawn mid-hop (`airborne`) or grounded. The two
/// content rows ride the top two rows of the block when airborne and the bottom
/// two when grounded, so toggling `airborne` between frames makes the rabbit
/// bound; advancing `col` carries it across the screen. Styled magenta-bold like
/// the mascot. Used by the startup [`splash`](super::super::splash) screen,
/// which owns the motion (the bounce between the screen edges and the per-frame
/// hop) and calls this purely to draw a frame.
pub fn running_rabbit(col: usize, face_right: bool, airborne: bool) -> Vec<String> {
    let [ears, body] = if face_right {
        RUNNER_RIGHT
    } else {
        RUNNER_LEFT
    };
    let pad = " ".repeat(col);
    let ears = format!("{pad}{ears}");
    let body = format!("{pad}{body}");
    let rows = if airborne {
        [ears, body, String::new()]
    } else {
        [String::new(), ears, body]
    };
    rows.into_iter()
        .map(|row| style(row).magenta().bold().to_string())
        .collect()
}

/// One usagi "segment" of the multiplying conga line, as `(ears, face, feet)`.
/// Each row is exactly six display columns wide — using only width-1 glyphs (no
/// zero-width sound marks) — so the three rows tile into an aligned block no
/// matter how many rabbits line up.
const MULTIPLY_EARS: &str = " n_n  ";
const MULTIPLY_FACE: &str = "(｡･-･)";
const MULTIPLY_FEET: &str = " └┘   ";

/// A three-row line of `count` usagi standing shoulder to shoulder — the
/// "multiplying" rabbits. Each rabbit is a fixed-width segment, so the rows tile
/// into an aligned block; growing `count` between frames reads as the warren
/// filling up. The block is **anchored to the left edge**: the first rabbit
/// always holds column zero and each new one extends the line rightward, so the
/// rabbits already on screen never shift sideways as the warren grows (no layout
/// jump). Styled magenta-bold (the mascot's colour). A `count` of zero yields
/// three blank rows.
pub fn multiplying_rabbits(count: usize) -> Vec<String> {
    let rows = [
        MULTIPLY_EARS.repeat(count),
        MULTIPLY_FACE.repeat(count),
        MULTIPLY_FEET.repeat(count),
    ];
    rows.into_iter()
        .map(|row| style(row).magenta().bold().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rabbit_lines_are_three_centered_mascot_rows() {
        let lines = rabbit_lines(80);
        assert_eq!(lines.len(), 3);
        // The mascot face appears, and the block is indented (centred).
        assert!(lines.iter().any(|l| l.contains("(='-')")));
        assert!(lines[0].starts_with(' '));
    }

    #[test]
    fn rabbit_lines_at_indents_the_art_by_the_given_column() {
        // The art begins at exactly `col`, so a caller can glide the mascot across
        // the screen by advancing the column it draws at.
        let lines = rabbit_lines_at(10);
        for (raw, art) in lines.iter().zip(RABBIT) {
            let plain = console::strip_ansi_codes(raw).into_owned();
            // Each row is the column's indent followed by the raw art row.
            assert_eq!(plain, format!("{}{art}", " ".repeat(10)));
        }
        // Centring is just placement at the centred column.
        assert_eq!(
            rabbit_lines(80),
            rabbit_lines_at(super::super::centered_padding(80, rabbit_width())),
        );
    }

    #[test]
    fn rabbit_width_and_height_describe_the_art_block() {
        // The widest row is the feet (`o(_(")(")`), and the art spans three rows.
        assert_eq!(
            rabbit_width(),
            RABBIT.iter().map(|l| l.chars().count()).max().unwrap()
        );
        assert_eq!(rabbit_height(), 3);
    }

    #[test]
    fn farewell_lines_are_an_aligned_box_around_the_rabbit() {
        let lines = farewell_lines();
        // A top and bottom rule frame every art row.
        assert_eq!(lines.len(), FAREWELL_ART.len() + 2);
        // The parting words sit inside the box.
        assert!(lines.iter().any(|l| l.contains("またね、ぴょん！")));
        // Strip the ANSI colours to inspect the box's shape.
        let plain: Vec<String> = lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        // Top and bottom are rounded corners; every row between has side borders.
        assert!(plain[0].starts_with('╭') && plain[0].ends_with('╮'));
        assert!(plain.last().unwrap().starts_with('╰') && plain.last().unwrap().ends_with('╯'));
        assert!(plain[1..plain.len() - 1]
            .iter()
            .all(|l| l.starts_with('│') && l.ends_with('│')));
        // Every row is the same display width, so the right edge lines up.
        let width = console::measure_text_width(&plain[0]);
        assert!(plain
            .iter()
            .all(|l| console::measure_text_width(l) == width));
    }

    #[test]
    fn workspace_rabbit_keeps_the_mascots_ears_and_feet_across_moods() {
        // Only the face row changes between moods; the ears (row 0) and feet
        // (row 2) stay the mascot's own, so the usagi reads as the same animal
        // whichever mode it reflects.
        for mood in [
            RabbitMood::Browsing,
            RabbitMood::Attentive,
            RabbitMood::Working,
        ] {
            let lines = workspace_rabbit(mood);
            assert_eq!(lines.len(), 3);
            let plain: Vec<String> = lines
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .collect();
            assert!(plain[0].contains("(\\(\\"), "ears on row 0 for {mood:?}");
            assert!(
                plain[2].contains("o(_(\")(\")"),
                "feet on row 2 for {mood:?}"
            );
        }
    }

    #[test]
    fn workspace_rabbit_changes_face_and_gesture_with_the_mood() {
        // Each mood shows a distinct expression and gesture, so the resting rabbit
        // signals the current engagement mode at a glance.
        let face =
            |mood| console::strip_ansi_codes(&workspace_rabbit(mood).join("\n")).into_owned();
        assert!(face(RabbitMood::Browsing).contains("(o.o)?"));
        assert!(face(RabbitMood::Attentive).contains("(^.^)/"));
        assert!(face(RabbitMood::Working).contains("(>.<)9"));
    }

    #[test]
    fn workspace_rabbit_sits_the_ears_over_the_head() {
        // The ears must start at the same column as the face's head (`(>.<)`), so
        // the left ear lands on the head rather than leaning onto the side paw.
        for mood in [
            RabbitMood::Browsing,
            RabbitMood::Attentive,
            RabbitMood::Working,
        ] {
            let lines = workspace_rabbit(mood);
            let plain: Vec<String> = lines
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .collect();
            let ear_col = plain[0].find('(').expect("ears have an opening paren");
            let head_col = plain[1].find('(').expect("head has an opening paren");
            assert_eq!(ear_col, head_col, "ears sit over the head for {mood:?}");
        }
    }

    #[test]
    fn workspace_rabbit_lines_up_the_body_under_the_head() {
        // The body's `(` must share the head's (and ears') `(` column, so the
        // ears, head, and body stack as one rabbit rather than the body trailing a
        // column to the right.
        for mood in [
            RabbitMood::Browsing,
            RabbitMood::Attentive,
            RabbitMood::Working,
        ] {
            let plain: Vec<String> = workspace_rabbit(mood)
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .collect();
            let head_col = plain[1].find('(').expect("head has an opening paren");
            let body_col = plain[2].find('(').expect("body has an opening paren");
            assert_eq!(
                body_col, head_col,
                "body lines up under the head for {mood:?}"
            );
        }
    }

    #[test]
    fn workspace_rabbit_rows_share_one_block_width() {
        // Every row pads to the widest, so the block tiles as a rectangle and the
        // advertised width matches what is drawn.
        for mood in [
            RabbitMood::Browsing,
            RabbitMood::Attentive,
            RabbitMood::Working,
        ] {
            let lines = workspace_rabbit(mood);
            let w0 = console::measure_text_width(&lines[0]);
            assert!(lines.iter().all(|l| console::measure_text_width(l) == w0));
            assert_eq!(w0, workspace_rabbit_width());
        }
    }

    #[test]
    fn workspace_rabbit_speaking_puts_the_speech_in_a_bubble_above_the_mascot() {
        let lines = workspace_rabbit_speaking(
            RabbitMood::Browsing,
            &["アップデートがあるぴょん".to_string(), "v0.2.0".to_string()],
            40,
        );
        let plain: Vec<String> = lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        let joined = plain.join("\n");
        // The bubble carries both speech lines, framed and tailed toward the rabbit.
        assert!(joined.contains("アップデートがあるぴょん"));
        assert!(joined.contains("v0.2.0"));
        assert!(plain[0].starts_with('╭') && plain[0].contains('╮'));
        assert!(
            joined.contains('┬'),
            "the bubble has a tail toward the rabbit"
        );
        // The mascot rests below the bubble: its ears and feet are the last rows.
        assert!(plain[plain.len() - 3].contains("(\\(\\"));
        assert!(plain.last().unwrap().contains("o(_(\")(\")"));
    }

    #[test]
    fn workspace_rabbit_speaking_rows_share_one_block_width() {
        // Every row pads to the widest, so the block tiles as a rectangle wherever
        // it is dropped into the sidebar.
        let lines = workspace_rabbit_speaking(
            RabbitMood::Attentive,
            &["アップデートがあるぴょん".to_string(), "v1.2.3".to_string()],
            40,
        );
        let w0 = console::measure_text_width(&lines[0]);
        assert!(lines.iter().all(|l| console::measure_text_width(l) == w0));
    }

    #[test]
    fn workspace_rabbit_speaking_wraps_a_long_message_to_fit_a_narrow_sidebar() {
        // A bubble narrower than the message wraps it onto more rows rather than
        // overrunning, and never exceeds the sidebar width.
        let max = 16;
        let lines = workspace_rabbit_speaking(
            RabbitMood::Working,
            &["アップデートがあるぴょん".to_string(), "v0.2.0".to_string()],
            max,
        );
        assert!(lines.iter().all(|l| console::measure_text_width(l) <= max));
        // More rows than the un-wrapped block (top + 2 speech + bottom + 3 rabbit).
        assert!(lines.len() > 6);
    }

    #[test]
    fn workspace_rabbit_speaking_falls_back_to_the_silent_mascot_when_too_narrow() {
        // No room for even a one-column bubble: it rests silently, exactly like
        // `workspace_rabbit`, rather than drawing a broken frame.
        let lines = workspace_rabbit_speaking(RabbitMood::Browsing, &["x".to_string()], 2);
        assert_eq!(lines, workspace_rabbit(RabbitMood::Browsing));
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
    fn running_rabbit_faces_its_direction_of_travel() {
        // Speed lines trail behind: on the left heading right, on the right
        // heading left. The face keeps its single `ㅅ` nose either way.
        let right =
            console::strip_ansi_codes(&running_rabbit(0, true, true).join("\n")).into_owned();
        assert!(right.contains("ﾐ(･ㅅ･)"));
        let left =
            console::strip_ansi_codes(&running_rabbit(0, false, true).join("\n")).into_owned();
        assert!(left.contains("(･ㅅ･)ﾐ"));
    }

    #[test]
    fn running_rabbit_is_three_rows_and_carries_the_offset() {
        // Always a three-row block; a larger `col` indents the art further so it
        // travels rightward across the screen.
        let near = running_rabbit(2, true, true);
        let far = running_rabbit(20, true, true);
        assert_eq!(near.len(), 3);
        assert_eq!(far.len(), 3);
        let lead = |line: &str| {
            console::strip_ansi_codes(line)
                .chars()
                .take_while(|c| *c == ' ')
                .count()
        };
        assert!(lead(&far[0]) > lead(&near[0]));
    }

    #[test]
    fn running_rabbit_bobs_between_the_top_and_bottom_rows() {
        // Airborne: the art rides the top two rows, leaving the last blank.
        // Grounded: it drops to the bottom two rows, leaving the first blank. So
        // toggling `airborne` between frames bounces the rabbit.
        let air = running_rabbit(0, true, true);
        assert!(console::strip_ansi_codes(&air[0]).contains('∩'));
        assert!(console::strip_ansi_codes(&air[2]).trim().is_empty());

        let ground = running_rabbit(0, true, false);
        assert!(console::strip_ansi_codes(&ground[0]).trim().is_empty());
        assert!(console::strip_ansi_codes(&ground[2]).contains('ㅅ'));
    }

    #[test]
    fn running_rabbit_keeps_the_ears_over_the_head_in_both_directions() {
        // The first ear must sit over the head centre (`ㅅ`) regardless of which
        // way the rabbit faces, so the ears never drift off the head.
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for face_right in [true, false] {
            let rows = running_rabbit(3, face_right, true);
            assert_eq!(
                col_of(&rows[0], '∩'),
                col_of(&rows[1], 'ㅅ'),
                "ears must sit over the head (face_right={face_right})",
            );
        }
    }

    #[test]
    fn running_rabbit_width_spans_the_widest_sprite_row() {
        // The bound a caller uses for the rabbit's travel matches the actual art:
        // the widest content row (`ﾐ(･ㅅ･)` / `(･ㅅ･)ﾐ`, seven columns).
        assert_eq!(running_rabbit_width(), 7);
    }

    #[test]
    fn multiplying_rabbits_lines_up_count_usagi() {
        // The face appears once per rabbit, so the warren grows with `count`.
        let plain = console::strip_ansi_codes(&multiplying_rabbits(3).join("\n")).into_owned();
        assert_eq!(plain.matches("(｡･-･)").count(), 3);
    }

    #[test]
    fn multiplying_rabbits_rows_stay_aligned_as_a_block() {
        // All three rows tile to the same width, so the ears/face/feet line up no
        // matter how many rabbits stand together.
        let lines = multiplying_rabbits(4);
        assert_eq!(lines.len(), 3);
        let w0 = console::measure_text_width(&lines[0]);
        assert!(lines.iter().all(|l| console::measure_text_width(l) == w0));
    }

    #[test]
    fn multiplying_rabbits_grow_wider_with_the_count() {
        // One more rabbit is one more fixed-width segment, so the block widens.
        let two = console::measure_text_width(&multiplying_rabbits(2)[1]);
        let five = console::measure_text_width(&multiplying_rabbits(5)[1]);
        assert!(five > two);
    }

    #[test]
    fn multiplying_rabbits_zero_count_is_blank() {
        // No rabbits yet: three empty rows (the animation starts from nothing).
        let lines = multiplying_rabbits(0);
        assert!(lines
            .iter()
            .all(|l| console::strip_ansi_codes(l).trim().is_empty()));
    }

    #[test]
    fn multiplying_rabbits_anchor_left_so_growth_never_shifts_them() {
        // The block is anchored to the left edge and a growing warren only appends
        // to the right: each row of the larger count starts with the row of the
        // smaller count, so the rabbits already on screen never jump sideways (no
        // layout shift). The first rabbit's face is flush left (column zero).
        let one = console::strip_ansi_codes(&multiplying_rabbits(1).join("\n")).into_owned();
        let three = console::strip_ansi_codes(&multiplying_rabbits(3).join("\n")).into_owned();
        for (small, big) in one.lines().zip(three.lines()) {
            assert!(big.starts_with(small), "growth must extend rightward only");
        }
        // The face row leads with the first rabbit's face, no centring padding.
        assert!(three.lines().nth(1).unwrap().starts_with("(｡･-･)"));
    }
}
