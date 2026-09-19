//! Small, deterministic fuzzy matching shared by TUI pickers.

/// A ranked match together with the candidate positions the query landed on.
///
/// Positions are `char` indices into the original candidate in ascending order,
/// so a picker can highlight exactly the cells the query explains. Scores are
/// compared only between candidates of the same query; their absolute value
/// carries no meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FuzzyMatch {
    score: usize,
    positions: Vec<usize>,
}

impl FuzzyMatch {
    /// Rank of this match; lower sorts first.
    pub(crate) const fn score(&self) -> usize {
        self.score
    }

    /// Take the ascending `char` indices of the candidate that the query
    /// matched, leaving the score behind.
    pub(crate) fn into_positions(self) -> Vec<usize> {
        self.positions
    }
}

/// Rank a case-insensitive subsequence match.
///
/// Contiguous matches sort before gapped matches, then shorter gaps and earlier
/// starts win. Callers retain source order as the final stable tie-break.
#[must_use]
pub(crate) fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    fuzzy_match(candidate, query).map(|matched| matched.score)
}

/// Rank a case-insensitive subsequence match and report where it landed.
///
/// This is [`fuzzy_score`] with the matched positions retained; both share one
/// implementation so a picker that highlights cannot drift from one that only
/// ranks. An empty query matches everything at score `0` with no positions.
#[must_use]
pub(crate) fn fuzzy_match(candidate: &str, query: &str) -> Option<FuzzyMatch> {
    let query = query.chars().map(lowercase).collect::<Vec<_>>();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let candidate = candidate.chars().map(lowercase).collect::<Vec<_>>();
    if let Some(start) = contiguous_start(&candidate, &query) {
        return Some(FuzzyMatch {
            score: start,
            positions: (start..start + query.len()).collect(),
        });
    }

    let mut positions = Vec::new();
    let mut wanted = 0;
    for (position, character) in candidate.iter().enumerate() {
        if *character != query[wanted] {
            continue;
        }
        positions.push(position);
        wanted += 1;
        if wanted == query.len() {
            let start = positions[0];
            let gaps = position + 1 - start - positions.len();
            return Some(FuzzyMatch {
                score: candidate.len() + gaps * 4 + start,
                positions,
            });
        }
    }
    None
}

/// Rank a repository path against a picker query.
///
/// A path picker is read as file names: a query that lands inside the name
/// always outranks one that only lands in a directory, however contiguous the
/// directory match is. Within that, contiguity, word boundaries (`/` `_` `-`
/// `.` and camelCase), an early start, and a shorter candidate each rank higher.
/// Case folds unless the query itself carries an uppercase letter (smart case).
///
/// The scan restarts at each occurrence of the query's first character, capped
/// at [`MAX_MATCH_STARTS`], so an early greedy choice cannot hide the match a
/// reader means.
#[must_use]
pub(crate) fn path_match(candidate: &str, query: &str) -> Option<FuzzyMatch> {
    let query = query.chars().collect::<Vec<_>>();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let fold = !query.iter().any(|character| character.is_uppercase());
    let candidate = candidate.chars().collect::<Vec<_>>();
    let name_start = candidate
        .iter()
        .rposition(|character| *character == '/')
        .map_or(0, |index| index + 1);

    // 名前側の start を先に試す。1 つでも当たればディレクトリ側の一致は必ず
    // 下位なので試さない。深い path で先頭側の一致が [`MAX_MATCH_STARTS`] を
    // 埋め、名前側の一致が捨てられるのも防ぐ。
    let starts = |from: usize, to: usize| {
        candidate[from..to]
            .iter()
            .enumerate()
            .filter(|(_, character)| same_cell(**character, query[0], fold))
            .map(move |(index, _)| from + index)
            .take(MAX_MATCH_STARTS)
    };
    best_from(
        &candidate,
        &query,
        fold,
        name_start,
        starts(name_start, candidate.len()),
    )
    .or_else(|| best_from(&candidate, &query, fold, name_start, starts(0, name_start)))
}

/// Best match reachable from any of `starts`.
fn best_from(
    candidate: &[char],
    query: &[char],
    fold: bool,
    name_start: usize,
    starts: impl Iterator<Item = usize>,
) -> Option<FuzzyMatch> {
    let mut best: Option<FuzzyMatch> = None;
    let mut positions = Vec::with_capacity(query.len());
    for start in starts {
        if !greedy_from(candidate, query, fold, start, &mut positions) {
            // 後ろの start ほど到達しにくいので、失敗したらそこで打ち切る。
            break;
        }
        let score = path_score(candidate, &positions, name_start);
        if best.as_ref().is_none_or(|previous| score < previous.score) {
            best = Some(FuzzyMatch {
                score,
                positions: positions.clone(),
            });
        }
    }
    best
}

/// Restarts tried per candidate before the first greedy match is accepted.
const MAX_MATCH_STARTS: usize = 16;
/// Rank floor of a match that reaches outside the file name. Every name match
/// ranks below it, so a directory hit can never outrank a name hit.
const DIRECTORY_RANK: usize = 1_000_000;
/// Rank added for leaving a gap before a matched cell.
const JUMP_PENALTY: usize = 2;
/// Rank added per skipped cell when the jump does not land on a word start.
const GAP_PENALTY: usize = 8;

/// Match `query` against `candidate` from `start`, taking the earliest cell for
/// each remaining query character.
///
/// The matched cells are written into `positions`, which the caller reuses
/// across restarts instead of allocating one vector per attempt.
fn greedy_from(
    candidate: &[char],
    query: &[char],
    fold: bool,
    start: usize,
    positions: &mut Vec<usize>,
) -> bool {
    positions.clear();
    let mut wanted = 0;
    for (position, character) in candidate.iter().enumerate().skip(start) {
        if !same_cell(*character, query[wanted], fold) {
            continue;
        }
        positions.push(position);
        wanted += 1;
        if wanted == query.len() {
            return true;
        }
    }
    false
}

/// Rank one accepted match; lower sorts first.
///
/// The cost is paid per step rather than over the whole span: a jump that lands
/// on a word start is nearly free, which is what makes an initialism like `pm`
/// pick `preview_modal.rs` over `parameters.rs`. A match that reaches outside
/// the file name is lifted past [`DIRECTORY_RANK`] instead of being scored
/// against name matches at all.
fn path_score(candidate: &[char], positions: &[usize], name_start: usize) -> usize {
    let first = positions[0];
    let mut cost = candidate.len() / 8;
    cost += first.saturating_sub(name_start);
    for pair in positions.windows(2) {
        let (previous, current) = (pair[0], pair[1]);
        if current == previous + 1 {
            continue;
        }
        cost += JUMP_PENALTY;
        if !starts_a_word(candidate[current - 1], candidate[current]) {
            cost += (current - previous - 1) * GAP_PENALTY;
        }
    }
    let cost = cost.min(DIRECTORY_RANK - 1);
    if first >= name_start {
        cost
    } else {
        DIRECTORY_RANK + cost
    }
}

/// Whether `current`, preceded by `previous`, begins a path segment or a word.
fn starts_a_word(previous: char, current: char) -> bool {
    matches!(previous, '/' | '_' | '-' | '.' | ' ')
        || (previous.is_lowercase() && current.is_uppercase())
}

/// Compare two cells, folding case unless the query asked for it.
fn same_cell(candidate: char, query: char, fold: bool) -> bool {
    if fold {
        lowercase(candidate) == lowercase(query)
    } else {
        candidate == query
    }
}

/// First index where `query` appears as a contiguous run inside `candidate`.
fn contiguous_start(candidate: &[char], query: &[char]) -> Option<usize> {
    if query.len() > candidate.len() {
        return None;
    }
    candidate
        .windows(query.len())
        .position(|window| window == query)
}

/// Lowercase one character for comparison, keeping it when the mapping is not a
/// single character (the picker compares cell by cell).
fn lowercase(character: char) -> char {
    let mut lowered = character.to_lowercase();
    match (lowered.next(), lowered.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

#[cfg(test)]
mod tests {
    use super::{FuzzyMatch, fuzzy_match, fuzzy_score, path_match};

    fn cells(matched: FuzzyMatch) -> Vec<usize> {
        matched.into_positions()
    }

    #[test]
    fn an_empty_query_matches_everything_without_positions() {
        let matched = fuzzy_match("crates/tui/src/lib.rs", "").unwrap();
        assert_eq!(matched.score(), 0);
        assert!(cells(matched).is_empty());
    }

    #[test]
    fn a_contiguous_run_reports_its_own_cells() {
        let matched = fuzzy_match("crates/tui/src/lib.rs", "tui").unwrap();
        assert_eq!(matched.score(), 7);
        assert_eq!(cells(matched), [7, 8, 9]);
        assert_eq!(fuzzy_score("crates/tui/src/lib.rs", "tui"), Some(7));
    }

    #[test]
    fn a_gapped_match_reports_each_matched_cell() {
        let matched = fuzzy_match("preview_modal.rs", "pmod").unwrap();
        // 連続一致より後ろに並ぶ。
        assert!(matched.score() > fuzzy_match("preview_modal.rs", "modal").unwrap().score());
        assert_eq!(cells(matched), [0, 8, 9, 10]);
    }

    #[test]
    fn case_and_width_fold_before_comparing() {
        assert_eq!(
            cells(fuzzy_match("README.md", "readme").unwrap()),
            [0, 1, 2, 3, 4, 5]
        );
        // 1 文字に畳めない大文字はそのまま比較する（İ は 2 文字へ展開される）。
        assert!(fuzzy_match("İstanbul.md", "İst").is_some());
    }

    #[test]
    fn a_query_longer_than_the_candidate_or_out_of_order_does_not_match() {
        assert_eq!(fuzzy_match("a.rs", "abcdefg"), None);
        assert_eq!(fuzzy_score("cleanup", "xyz"), None);
    }

    fn ranked<'a>(query: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        let mut ranked = candidates
            .iter()
            .enumerate()
            .filter_map(|(order, candidate)| {
                path_match(candidate, query).map(|matched| (matched.score(), order, *candidate))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(score, order, _)| (*score, *order));
        ranked
            .into_iter()
            .map(|(_, _, candidate)| candidate)
            .collect()
    }

    #[test]
    fn a_name_match_outranks_a_directory_match() {
        assert_eq!(
            ranked(
                "preview",
                &[
                    "crates/tui/src/usecase/application/controller/preview/mod.rs",
                    "crates/tui/src/presentation/views/preview_modal.rs",
                ]
            ),
            [
                "crates/tui/src/presentation/views/preview_modal.rs",
                "crates/tui/src/usecase/application/controller/preview/mod.rs",
            ]
        );
    }

    #[test]
    fn boundaries_contiguity_and_length_order_the_rest() {
        // `_` の直後は人が略語を作る位置なので、境界に当たる方が上に来る。
        assert_eq!(
            ranked("pm", &["src/preview_modal.rs", "src/parameters.rs"]),
            ["src/preview_modal.rs", "src/parameters.rs"]
        );
        // 同じ当たり方なら短い候補が上。
        assert_eq!(
            ranked(
                "preview",
                &["src/preview_modal_regression_test.rs", "src/preview.rs",]
            ),
            ["src/preview.rs", "src/preview_modal_regression_test.rs"]
        );
        // 早い greedy 一致に隠れず、連続一致を選び直す。
        assert_eq!(cells(path_match("aXbXabc", "abc").unwrap()), [4, 5, 6]);
    }

    #[test]
    fn a_deep_path_still_matches_inside_its_file_name() {
        // 先頭文字がディレクトリ側に 16 か所以上あっても、名前側の一致を捨てない。
        let deep = "a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/abc.rs";
        let matched = path_match(deep, "abc").unwrap();
        let name_start = deep.rfind('/').unwrap() + 1;
        assert!(matched.score() < super::DIRECTORY_RANK);
        assert_eq!(cells(matched), [name_start, name_start + 1, name_start + 2]);

        // 名前側に無ければディレクトリ側へ落ちる。
        let matched = path_match("abc/def/ghi.rs", "abc").unwrap();
        assert!(matched.score() >= super::DIRECTORY_RANK);
        assert_eq!(cells(matched), [0, 1, 2]);
    }

    #[test]
    fn an_uppercase_query_turns_the_match_case_sensitive() {
        assert!(path_match("src/readme.md", "README").is_none());
        assert!(path_match("src/README.md", "README").is_some());
        assert!(path_match("src/README.md", "readme").is_some());
    }

    #[test]
    fn an_empty_query_keeps_every_path_at_rank_zero() {
        let matched = path_match("src/lib.rs", "").unwrap();
        assert_eq!(matched.score(), 0);
        assert!(cells(matched).is_empty());
        assert_eq!(path_match("src/lib.rs", "zz"), None);
    }
}
