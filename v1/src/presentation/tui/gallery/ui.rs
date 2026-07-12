use anyhow::Result;

use crate::presentation::tui::widgets;

/// Columns the running rabbit advances per frame in the gallery, so it bounds
/// briskly from edge to edge.
const RUN_STEP: usize = 3;
/// Frames between each new rabbit joining the multiplying line.
const MULTIPLY_GROW: usize = 3;
/// Frames between each expression change of the face-cycling loader.
const FACE_DIV: usize = 8;
/// Upper bound on how many rabbits the multiplying line grows to.
const MULTIPLY_MAX: usize = 8;

/// The rabbit animations the `usagi run <N>` gallery can play, one per number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variation {
    /// 1 — the startup splash: a usagi bounding back and forth across the screen.
    Running,
    /// 2 — usagi lining up and multiplying across the screen.
    Multiplying,
    /// 3 — the two-line loader: a hopping usagi with a braille spinner.
    LoadingHop,
    /// 4 — the two-line loader whose expression drifts on a timer.
    LoadingFaces,
    /// 5 — the static welcome mascot.
    Mascot,
}

impl Variation {
    /// Maps the `usagi run <N>` argument to a variation, rejecting out-of-range
    /// numbers with a hint about the valid set.
    pub fn from_number(n: u8) -> Result<Self> {
        Ok(match n {
            1 => Self::Running,
            2 => Self::Multiplying,
            3 => Self::LoadingHop,
            4 => Self::LoadingFaces,
            5 => Self::Mascot,
            other => {
                anyhow::bail!("うさぎは 1〜5 で指定してください（指定された番号: {other}）")
            }
        })
    }

    /// The label shown in the gallery footer.
    fn name(self) -> &'static str {
        match self {
            Self::Running => "1/5 走り回るうさぎ",
            Self::Multiplying => "2/5 増えていくうさぎ",
            Self::LoadingHop => "3/5 読み込み（ホップ＋スピナー）",
            Self::LoadingFaces => "4/5 読み込み（表情が変わる）",
            Self::Mascot => "5/5 マスコット",
        }
    }
}

/// Centres each of `rows` horizontally within `width` by sharing one left
/// padding (measured by display width, so styled rows still align).
fn center_horizontally(rows: Vec<String>, width: usize) -> Vec<String> {
    let block_w = rows
        .iter()
        .map(|r| console::measure_text_width(r))
        .max()
        .unwrap_or(0);
    let pad = " ".repeat(widgets::centered_padding(width, block_w));
    rows.into_iter().map(|r| format!("{pad}{r}")).collect()
}

/// The moving rows for `variation` at `frame`, before vertical centring.
fn body(variation: Variation, frame: usize, width: usize) -> Vec<String> {
    match variation {
        Variation::Running => {
            // A continuous edge-to-edge bounce: a triangle wave over the track,
            // facing the way it travels, hopping each frame.
            let span = width.saturating_sub(widgets::running_rabbit_width());
            let period = (2 * span).max(1);
            let phase = (frame * RUN_STEP) % period;
            let (col, face_right) = if phase < span {
                (phase, true)
            } else {
                (period - phase, false)
            };
            widgets::running_rabbit(col, face_right, frame.is_multiple_of(2))
        }
        Variation::Multiplying => {
            let count = 1 + (frame / MULTIPLY_GROW) % MULTIPLY_MAX;
            widgets::multiplying_rabbits(count)
        }
        Variation::LoadingHop => {
            center_horizontally(widgets::loading_rabbit(frame, "読み込み中…"), width)
        }
        Variation::LoadingFaces => center_horizontally(
            widgets::loading_rabbit_timed(frame, frame / FACE_DIV, "導入中…"),
            width,
        ),
        Variation::Mascot => widgets::rabbit_lines(width),
    }
}

/// Builds the gallery frame for `variation` at `frame` and a raw terminal size:
/// the animation centred vertically, with a dim footer naming the variation and
/// the exit hint pinned to the bottom row.
pub fn render_frame(
    variation: Variation,
    frame: usize,
    raw_height: usize,
    raw_width: usize,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let art = body(variation, frame, width);
    let footer = widgets::dim_line(width, &format!("{}  |  なにかキーで終了", variation.name()));

    let mut lines = Vec::with_capacity(height);
    // Centre the art in the space above the footer row.
    let top_padding = height.saturating_sub(art.len() + 1) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(art);
    // Pad down to the last row, then pin the footer there.
    while lines.len() + 1 < height {
        lines.push(String::new());
    }
    lines.push(footer);
    // The art block was padded to at least `height - 1` rows before the footer,
    // so `lines` is now at least `height`; clip a tall animation back to fit.
    lines.truncate(height);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_number_maps_each_variation() {
        assert_eq!(Variation::from_number(1).unwrap(), Variation::Running);
        assert_eq!(Variation::from_number(2).unwrap(), Variation::Multiplying);
        assert_eq!(Variation::from_number(3).unwrap(), Variation::LoadingHop);
        assert_eq!(Variation::from_number(4).unwrap(), Variation::LoadingFaces);
        assert_eq!(Variation::from_number(5).unwrap(), Variation::Mascot);
    }

    #[test]
    fn from_number_rejects_out_of_range() {
        let err = Variation::from_number(0).unwrap_err();
        assert!(err.to_string().contains("1〜5"));
        assert!(Variation::from_number(6).is_err());
    }

    #[test]
    fn render_frame_fills_the_terminal_and_names_the_variation() {
        for n in 1..=5u8 {
            let v = Variation::from_number(n).unwrap();
            let frame = render_frame(v, 3, 24, 80);
            assert_eq!(frame.len(), 24);
            let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
            assert!(joined.contains(v.name()), "footer names variation {n}");
            assert!(joined.contains("なにかキーで終了"));
        }
    }

    #[test]
    fn running_variation_bounces_and_faces_both_ways() {
        // Over a full cycle the rabbit faces right on the way out and left on the
        // way back, so both sprites appear.
        let mut seen_right = false;
        let mut seen_left = false;
        for frame in 0..200 {
            let plain = console::strip_ansi_codes(
                &render_frame(Variation::Running, frame, 24, 80).join("\n"),
            )
            .into_owned();
            seen_right |= plain.contains("ﾐ(･ㅅ･)");
            seen_left |= plain.contains("(･ㅅ･)ﾐ");
        }
        assert!(seen_right && seen_left);
    }

    #[test]
    fn multiplying_variation_grows_then_resets() {
        // The line starts at one rabbit and grows; later frames hold more than
        // the first frame does.
        let count = |frame| {
            console::strip_ansi_codes(
                &render_frame(Variation::Multiplying, frame, 24, 80).join("\n"),
            )
            .matches("(｡･-･)")
            .count()
        };
        assert_eq!(count(0), 1);
        assert!(count(MULTIPLY_GROW * 3) > count(0));
    }

    #[test]
    fn loading_variations_render_the_loader_faces() {
        let hop =
            console::strip_ansi_codes(&render_frame(Variation::LoadingHop, 0, 24, 80).join("\n"))
                .into_owned();
        assert!(hop.contains("読み込み中…"));
        let faces =
            console::strip_ansi_codes(&render_frame(Variation::LoadingFaces, 0, 24, 80).join("\n"))
                .into_owned();
        assert!(faces.contains("導入中…"));
    }

    #[test]
    fn mascot_variation_shows_the_welcome_art() {
        let plain =
            console::strip_ansi_codes(&render_frame(Variation::Mascot, 0, 24, 80).join("\n"))
                .into_owned();
        assert!(plain.contains("(='-')"));
    }

    #[test]
    fn render_frame_never_overflows_a_short_terminal() {
        // A terminal too short for art + footer still returns exactly `height`
        // rows and never panics.
        let frame = render_frame(Variation::Mascot, 0, 2, 80);
        assert_eq!(frame.len(), 2);
    }
}

#[cfg(test)]
mod _dbg {
    use super::*;
    #[test]
    fn _dump() {
        for f in [0usize, 1] {
            let rows = render_frame(Variation::LoadingHop, f, 12, 70);
            let non: Vec<String> = rows
                .iter()
                .map(|r| console::strip_ansi_codes(r).trim_end().to_string())
                .filter(|s| !s.trim().is_empty())
                .collect();
            eprintln!("frame {f}: {} non-empty rows:", non.len());
            for r in non {
                eprintln!("  [{r}]");
            }
        }
    }
}
