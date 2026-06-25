//! The `usagi icon` command: print the square-pixel usagi marks to stdout.
//!
//! The marks are designed on a grid of unit squares (the Claude Code logo
//! technique): a row of `'#'` (filled) / `'.'` (empty) cells, all rows the same
//! width. Block characters draw the grid, and a horizontal flip mirrors a
//! one-directional silhouette:
//!
//! - `flip` — a side profile and its horizontal mirror, packed into quadrant
//!   glyphs (`▘▝▖▗…`, a 2×2 cell block per character), side by side.
//! - `half` — the front-facing head packed into half squares (`▀▄`, two grid
//!   rows per text row), halving the height.
//!
//! Like [`super::feature`], the lines are built by a pure [`render`] (unit
//! tested) and a thin [`run`] prints them.

use anyhow::Result;
use clap::ValueEnum;

/// A pixel grid: each row is `'#'` (filled square) / `'.'` (empty), every row the
/// same width.
type Grid = &'static [&'static str];

/// Side profile facing right (ears and nose to the right, tail to the left). The
/// hind leg is tucked under the rounded rump, so it is not drawn; only a small
/// front paw shows under the chest, set off from the rump by a gap and resting on
/// the same ground line. Not symmetric — [`flip`] mirrors it to face the other
/// way.
const PROFILE: Grid = &[
    ".........##.##..",
    ".........##.##..",
    ".........#####..",
    ".........######.",
    "........########",
    "..###########.##",
    ".###############",
    "###############.",
    "##############..",
    "#############...",
    ".#######..##....",
    "..######..##....",
];

/// Front-facing head: ears, head and two eyes, mirror-symmetric about its
/// vertical axis (each row is a palindrome).
const MINI: Grid = &[
    "..##..##..",
    "..##..##..",
    "..##..##..",
    ".########.",
    "##########",
    "##.####.##",
    "##########",
    ".########.",
    "..######..",
];

/// Which mark(s) `usagi icon` prints. `All` (the default) prints both sections.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum IconView {
    /// Both sections, top to bottom.
    All,
    /// A side profile and its horizontal mirror, side by side.
    Flip,
    /// The front-facing head packed into half squares.
    Half,
}

/// Entry point for `usagi icon [view]`: print the chosen mark(s).
pub fn run(view: IconView) -> Result<()> {
    for line in render(view) {
        println!("{line}");
    }
    Ok(())
}

/// The lines printed for `view` — a header per section followed by its art.
fn render(view: IconView) -> Vec<String> {
    match view {
        IconView::Flip => flip_section(),
        IconView::Half => half_section(),
        IconView::All => {
            let mut lines = flip_section();
            lines.push(String::new());
            lines.extend(half_section());
            lines
        }
    }
}

/// `flip`: the right-facing profile (the original) beside its horizontal mirror,
/// both packed into quadrant glyphs. Designing one direction and reflecting it
/// yields the other for free.
fn flip_section() -> Vec<String> {
    let right = quad_blocks(PROFILE);
    let left = quad_blocks(&flip(PROFILE));
    let mut lines = vec![
        "② 水平フリップ · 横向き — 原型を左右反転（▘▝ で小型化）".to_string(),
        String::new(),
    ];
    lines.extend(beside(&right, &left, 3));
    lines.push(String::new());
    lines.push("→ 右向き（原型）   ← 左向き（反転）".to_string());
    lines
}

/// `half`: the front-facing head packed into half squares (`▀▄`), folding two
/// grid rows into one text row so the mark is half as tall.
fn half_section() -> Vec<String> {
    let mut lines = vec!["中（半マス ▀▄・縦を半分に）".to_string(), String::new()];
    lines.extend(half_blocks(MINI));
    lines
}

/// Mirror each row left-to-right, turning a one-directional silhouette into its
/// reflection.
fn flip(grid: Grid) -> Vec<String> {
    grid.iter().map(|row| row.chars().rev().collect()).collect()
}

/// The 16 quadrant glyphs, indexed by the 2×2 block's filled corners as the bits
/// `tl<<3 | tr<<2 | bl<<1 | br` (top-left, top-right, bottom-left, bottom-right).
/// A `▝`-style character carries up to four sub-squares, so a 2×2 cell block
/// collapses into one terminal cell.
const QUAD: [char; 16] = [
    ' ', '▗', '▖', '▄', '▝', '▐', '▞', '▟', '▘', '▚', '▌', '▙', '▀', '▜', '▛', '█',
];

/// Pack each 2×2 block of cells into a single quadrant glyph from [`QUAD`],
/// halving the mark in **both** dimensions. Odd final rows or columns pair with
/// empty cells.
fn quad_blocks<S: AsRef<str>>(rows: &[S]) -> Vec<String> {
    let cell = |row: &[char], i: usize| row.get(i) == Some(&'#');
    let mut out = Vec::new();
    let mut y = 0;
    while y < rows.len() {
        let top: Vec<char> = rows[y].as_ref().chars().collect();
        let bottom: Vec<char> = rows
            .get(y + 1)
            .map(|r| r.as_ref().chars().collect())
            .unwrap_or_default();
        let width = top.len().max(bottom.len());
        let mut line = String::new();
        let mut x = 0;
        while x < width {
            let idx = (cell(&top, x) as usize) << 3
                | (cell(&top, x + 1) as usize) << 2
                | (cell(&bottom, x) as usize) << 1
                | (cell(&bottom, x + 1) as usize);
            line.push(QUAD[idx]);
            x += 2;
        }
        out.push(line);
        y += 2;
    }
    out
}

/// Pack two grid rows into one text row using half squares: for each column the
/// top and bottom cells pick `█` (both), `▀` (top only), `▄` (bottom only) or a
/// space (neither). An odd final row pairs with an empty bottom. This halves the
/// height, drawing the same mark at a smaller, still-legible size.
fn half_blocks<S: AsRef<str>>(rows: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let top: Vec<char> = rows[i].as_ref().chars().collect();
        let bottom: Vec<char> = rows
            .get(i + 1)
            .map(|r| r.as_ref().chars().collect())
            .unwrap_or_default();
        let width = top.len().max(bottom.len());
        let line: String = (0..width)
            .map(|x| {
                let t = top.get(x) == Some(&'#');
                let b = bottom.get(x) == Some(&'#');
                match (t, b) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                }
            })
            .collect();
        out.push(line);
        i += 2;
    }
    out
}

/// Lay two rendered blocks side by side with `gap` spaces between. The left block
/// is padded to its widest row so the right block's columns line up, and the
/// shorter block is padded with blank rows so every output row carries both.
fn beside(left: &[String], right: &[String], gap: usize) -> Vec<String> {
    let left_width = left.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let height = left.len().max(right.len());
    let spacer = " ".repeat(gap);
    (0..height)
        .map(|i| {
            let l = left.get(i).map(String::as_str).unwrap_or("");
            let r = right.get(i).map(String::as_str).unwrap_or("");
            let pad = " ".repeat(left_width - l.chars().count());
            format!("{l}{pad}{spacer}{r}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids_have_uniform_row_widths() {
        for grid in [PROFILE, MINI] {
            let width = grid[0].chars().count();
            assert!(grid.iter().all(|row| row.chars().count() == width));
        }
    }

    #[test]
    fn mini_rows_are_palindromes_so_the_head_is_mirror_symmetric() {
        for row in MINI {
            let reversed: String = row.chars().rev().collect();
            assert_eq!(*row, reversed, "mini row not symmetric: {row}");
        }
    }

    #[test]
    fn flip_reverses_every_row() {
        let flipped = flip(PROFILE);
        assert_eq!(flipped.len(), PROFILE.len());
        for (orig, rev) in PROFILE.iter().zip(&flipped) {
            let expected: String = orig.chars().rev().collect();
            assert_eq!(*rev, expected);
        }
        // The profile faces right; the flip does not equal the original, so
        // reflection genuinely turns it around.
        assert_ne!(flipped[0], PROFILE[0].to_string());
    }

    #[test]
    fn quad_blocks_maps_every_2x2_corner_combination() {
        // Lay out all 16 corner combinations across one row pair: column pair `i`
        // encodes the bits `tl tr / bl br` of index `i`, so the rendered glyph at
        // position `i` must be `QUAD[i]`.
        let mut top = String::new();
        let mut bottom = String::new();
        for idx in 0..16usize {
            top.push(if idx & 0b1000 != 0 { '#' } else { '.' });
            top.push(if idx & 0b0100 != 0 { '#' } else { '.' });
            bottom.push(if idx & 0b0010 != 0 { '#' } else { '.' });
            bottom.push(if idx & 0b0001 != 0 { '#' } else { '.' });
        }
        let glyphs: Vec<char> = quad_blocks(&[top, bottom])[0].chars().collect();
        for (idx, glyph) in QUAD.iter().enumerate() {
            assert_eq!(glyphs[idx], *glyph, "wrong glyph for corner bits {idx:04b}");
        }
    }

    #[test]
    fn quad_blocks_pads_odd_trailing_row_and_column() {
        // A lone top-left cell (odd height, odd width) pairs with empty neighbours,
        // yielding the top-left quadrant `▘`.
        assert_eq!(quad_blocks(&["#"]), vec!["▘".to_string()]);
    }

    #[test]
    fn half_blocks_covers_every_top_bottom_combination() {
        // Row pair exercises ▀ (top only), ▄ (bottom only), █ (both), space (none).
        let rendered = half_blocks(&["#.#.", ".##."]);
        assert_eq!(rendered, vec!["▀▄█ ".to_string()]);
    }

    #[test]
    fn half_blocks_pairs_an_odd_final_row_with_an_empty_bottom() {
        // A lone trailing row keeps its filled cells as ▀ (top only).
        let rendered = half_blocks(&["##"]);
        assert_eq!(rendered, vec!["▀▀".to_string()]);
    }

    #[test]
    fn half_blocks_halves_the_row_count() {
        // Two text rows for a five-row grid (MINI's 9 rows -> 5, rounding up).
        assert_eq!(half_blocks(MINI).len(), MINI.len().div_ceil(2));
    }

    #[test]
    fn beside_aligns_the_right_block_and_pads_the_shorter_one() {
        let left = vec!["█".to_string(), "███".to_string()];
        let right = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let joined = beside(&left, &right, 2);
        assert_eq!(
            joined,
            vec![
                "█    a".to_string(), // left padded to width 3, then 2-space gap
                "███  b".to_string(),
                "     c".to_string(), // left ran out: blank, padded, then the gap
            ]
        );
    }

    #[test]
    fn flip_section_shows_both_orientations_side_by_side() {
        let lines = flip_section();
        assert!(lines[0].contains("反転"));
        // The caption names both directions, and the art rows carry two blocks
        // (the original and its mirror) separated by the gap.
        assert!(lines
            .iter()
            .any(|l| l.contains("原型") && l.contains("反転")));
        let widest = quad_blocks(PROFILE)
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap();
        assert!(
            lines.iter().any(|l| l.chars().count() > widest),
            "no row spans both blocks"
        );
    }

    #[test]
    fn half_section_renders_the_head_in_half_squares() {
        let lines = half_section();
        assert!(lines[0].contains("半マス"));
        // The art is the half-square rendering of the head grid.
        assert_eq!(lines[2..].to_vec(), half_blocks(MINI));
    }

    #[test]
    fn render_all_includes_both_sections() {
        let all = render(IconView::All);
        assert!(all.iter().any(|l| l.contains("水平フリップ")));
        assert!(all.iter().any(|l| l.contains("半マス")));
    }

    #[test]
    fn render_selects_a_single_section() {
        assert_eq!(render(IconView::Flip), flip_section());
        assert_eq!(render(IconView::Half), half_section());
    }

    #[test]
    fn run_prints_each_view_without_error() {
        for view in [IconView::All, IconView::Flip, IconView::Half] {
            assert!(run(view).is_ok());
        }
    }
}
