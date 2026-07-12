use super::{PrLink, PrState};

#[test]
fn new_builds_an_open_untitled_link() {
    let pr = PrLink::new(412, "https://github.com/o/r/pull/412");
    assert_eq!(pr.number, 412);
    assert_eq!(pr.title, None);
    assert_eq!(pr.state, PrState::Open);
    assert!(!pr.pinned);
    assert_eq!(pr.attempts, 0);
    assert!(!pr.refreshing);
    assert!(pr.is_visible());
    assert!(!pr.is_dismissed());
}

#[test]
fn dismissed_pr_is_hidden_from_the_visible_count() {
    let mut a = PrLink::new(1, "https://x/pull/1");
    let b = PrLink::new(2, "https://x/pull/2");
    assert_eq!(PrLink::visible_count(&[a.clone(), b.clone()]), 2);
    a.state = PrState::Dismissed;
    assert!(a.is_dismissed());
    assert!(!a.is_visible());
    assert_eq!(PrLink::visible_count(&[a, b]), 1);
}

#[test]
fn pr_key_truncates_at_the_pull_number() {
    let plain = PrLink::new(412, "https://github.com/o/r/pull/412");
    let files = PrLink::new(412, "https://github.com/o/r/pull/412/files");
    let query = PrLink::new(412, "https://github.com/o/r/pull/412?w=1");
    assert_eq!(plain.pr_key(), "https://github.com/o/r/pull/412");
    assert_eq!(files.pr_key(), "https://github.com/o/r/pull/412");
    assert_eq!(query.pr_key(), "https://github.com/o/r/pull/412");
}

#[test]
fn pr_key_returns_the_whole_url_without_a_pull_segment() {
    // No `/pull/` marker at all.
    let none = PrLink::new(0, "https://example.com/issues/9");
    assert_eq!(none.pr_key(), "https://example.com/issues/9");
    // A `/pull/` with no digits after it is not a recognisable PR path.
    let no_digits = PrLink::new(0, "https://example.com/pull/x");
    assert_eq!(no_digits.pr_key(), "https://example.com/pull/x");
}

#[test]
fn aggregate_dedups_by_key_and_upgrades_a_missing_title() {
    let untitled = PrLink::new(412, "https://x/pull/412");
    let mut titled = PrLink::new(412, "https://x/pull/412/files");
    titled.title = Some("Add feature".to_string());

    let folded = PrLink::aggregate([untitled, titled]);
    assert_eq!(folded.len(), 1);
    // The first sighting keeps its URL but adopts the later title.
    assert_eq!(folded[0].url, "https://x/pull/412");
    assert_eq!(folded[0].title.as_deref(), Some("Add feature"));
}

#[test]
fn aggregate_keeps_the_first_title_when_both_are_set() {
    let mut first = PrLink::new(1, "https://x/pull/1");
    first.title = Some("First".to_string());
    let mut second = PrLink::new(1, "https://x/pull/1/files");
    second.title = Some("Second".to_string());

    let folded = PrLink::aggregate([first, second]);
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0].title.as_deref(), Some("First"));
}

#[test]
fn aggregate_keeps_distinct_prs_and_makes_dismissal_and_pin_sticky() {
    let a = PrLink::new(1, "https://x/pull/1");
    let mut a_dismissed = PrLink::new(1, "https://x/pull/1/files");
    a_dismissed.state = PrState::Dismissed;
    let mut a_pinned = PrLink::new(1, "https://x/pull/1?w=1");
    a_pinned.pinned = true;
    let b = PrLink::new(2, "https://x/pull/2");

    let folded = PrLink::aggregate([a, a_dismissed, a_pinned, b]);
    assert_eq!(folded.len(), 2);
    let one = folded.iter().find(|p| p.number == 1).unwrap();
    assert_eq!(one.state, PrState::Dismissed); // dismissal folded in
    assert!(one.pinned); // pin folded in
}

#[test]
fn aggregate_of_nothing_is_empty() {
    assert!(PrLink::aggregate([]).is_empty());
}

#[test]
fn state_default_is_open_and_serializes_snake_case() {
    assert_eq!(PrState::default(), PrState::Open);
    assert_eq!(serde_json::to_value(PrState::Merged).unwrap(), "merged");
    assert_eq!(
        serde_json::to_value(PrState::Dismissed).unwrap(),
        "dismissed"
    );
}

#[test]
fn state_degrades_an_unrecognised_token_to_open() {
    assert_eq!(
        serde_json::from_str::<PrState>("\"closed\"").unwrap(),
        PrState::Open
    );
    assert_eq!(
        serde_json::from_str::<PrState>("\"merged\"").unwrap(),
        PrState::Merged
    );
}

#[test]
fn open_pr_omits_the_defaulted_fields_but_merged_writes_state() {
    // An open, unpinned, never-failed PR omits state / pinned / attempts.
    let open = PrLink::new(1, "https://x/pull/1");
    let json = serde_json::to_string(&open).unwrap();
    assert!(!json.contains("state"), "{json}");
    assert!(!json.contains("pinned"), "{json}");
    assert!(!json.contains("attempts"), "{json}");
    assert!(!json.contains("refreshing"), "{json}");

    // A merged / pinned / failed PR writes those fields.
    let mut rich = PrLink::new(2, "https://x/pull/2");
    rich.state = PrState::Merged;
    rich.pinned = true;
    rich.attempts = 3;
    let json = serde_json::to_string(&rich).unwrap();
    assert!(json.contains("\"state\":\"merged\""));
    assert!(json.contains("\"pinned\":true"));
    assert!(json.contains("\"attempts\":3"));
}

#[test]
fn pr_link_round_trips_through_json_and_drops_the_transient_flag() {
    let mut pr = PrLink::new(9, "https://x/pull/9");
    pr.title = Some("T".to_string());
    pr.state = PrState::Merged;
    pr.refreshing = true; // transient: must not persist

    let json = serde_json::to_string(&pr).unwrap();
    let back: PrLink = serde_json::from_str(&json).unwrap();
    // `refreshing` resets to false on load regardless of the in-memory value.
    assert!(!back.refreshing);
    assert_eq!(back.number, 9);
    assert_eq!(back.title.as_deref(), Some("T"));
    assert_eq!(back.state, PrState::Merged);
    // Exercise the derived Clone / Debug.
    assert_eq!(pr.clone().number, 9);
    assert!(format!("{pr:?}").contains("pull/9"));
}
