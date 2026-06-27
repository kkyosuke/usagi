//! The full-text match rule shared by issue and memory search.
//!
//! Both surfaces fold the query and the candidate fields with Unicode-aware
//! [`str::to_lowercase`] and match on [`str::contains`], so the fold works for
//! the Japanese text the UI carries and a multi-byte needle can never match
//! across a character boundary (an ASCII byte-window match would get both
//! wrong). Keeping the rule here means issue and memory search can never quietly
//! diverge — a change to the matching (e.g. a different normalisation) updates
//! both at once.

/// Case-fold a search query once, so the (potentially many) per-item matches do
/// not each recompute it. Pass the result to [`matches_folded`].
pub fn fold_query(query: &str) -> String {
    query.to_lowercase()
}

/// Whether any `field` contains the already-folded `needle`. An empty needle
/// matches everything (an empty query lists all items).
pub fn matches_folded(needle: &str, fields: &[&str]) -> bool {
    needle.is_empty() || fields.iter().any(|f| contains_folded(f, needle))
}

/// Whether `field` contains the already-folded (lower-cased) `needle`,
/// case-insensitively. `needle` is assumed non-empty (the empty case is handled
/// by [`matches_folded`]).
///
/// When both sides are ASCII the fold is purely byte-level, so the field is
/// scanned for the needle in place rather than allocating a lower-cased copy of
/// the (potentially multi-KB) field on every call — the hot path, since search
/// re-runs as the user types and folds every candidate field each keystroke.
/// Any non-ASCII on either side falls back to the Unicode-aware fold, so a
/// multi-byte needle can never match across a character boundary (the reason an
/// unconditional byte-window match would be wrong — see the module docs).
fn contains_folded(field: &str, needle: &str) -> bool {
    if field.is_ascii() && needle.is_ascii() {
        let (field, needle) = (field.as_bytes(), needle.as_bytes());
        return needle.len() <= field.len()
            && field
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle));
    }
    field.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything() {
        let needle = fold_query("");
        assert!(matches_folded(&needle, &["anything"]));
        assert!(matches_folded(&needle, &[]));
    }

    #[test]
    fn matching_is_case_insensitive_across_fields() {
        let needle = fold_query("LOGIN");
        assert!(matches_folded(&needle, &["Fix the login flow", "body"]));
        assert!(matches_folded(&needle, &["title", "the LOGIN screen"]));
        assert!(!matches_folded(&needle, &["unrelated", "text"]));
    }

    #[test]
    fn matching_folds_non_ascii_text() {
        // A Japanese needle folds and matches without splitting a code point.
        let needle = fold_query("ログイン");
        assert!(matches_folded(&needle, &["ログイン画面の修正"]));
    }

    #[test]
    fn ascii_fast_path_handles_a_needle_longer_than_the_field() {
        // The in-place ASCII scan must not match (or panic) when the needle is
        // longer than the field.
        let needle = fold_query("login-flow-detail");
        assert!(!matches_folded(&needle, &["login"]));
    }

    #[test]
    fn non_ascii_field_takes_the_unicode_fallback() {
        // A non-ASCII field forces the Unicode fold even for an ASCII needle: a
        // present ASCII run matches, an absent needle does not.
        let present = fold_query("PR");
        assert!(matches_folded(&present, &["PR レビュー"]));
        let absent = fold_query("xyz");
        assert!(!matches_folded(&absent, &["日本語のみ"]));
    }
}
