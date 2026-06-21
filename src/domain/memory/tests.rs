use super::*;
use chrono::{TimeZone, Utc};

fn sample() -> Memory {
    let ts = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
    Memory {
        name: "user-prefers-tabs".to_string(),
        title: "ユーザーはタブを好む".to_string(),
        kind: MemoryType::User,
        related: vec!["editor-config".to_string()],
        created_at: ts,
        updated_at: ts,
        body: "本文。".to_string(),
    }
}

#[test]
fn type_tokens_round_trip() {
    for kind in [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ] {
        assert_eq!(kind.to_string(), kind.as_str());
        assert_eq!(kind.as_str().parse::<MemoryType>().unwrap(), kind);
    }
}

#[test]
fn type_default_is_project() {
    assert_eq!(MemoryType::default(), MemoryType::Project);
}

#[test]
fn type_parse_trims_and_rejects_unknown() {
    assert_eq!(
        "  feedback ".parse::<MemoryType>().unwrap(),
        MemoryType::Feedback
    );
    let err = "bogus".parse::<MemoryType>().unwrap_err();
    assert!(err.to_string().contains("invalid type"));
}

#[test]
fn slugify_lowercases_and_collapses() {
    assert_eq!(slugify("Hello, World!  Again"), "hello-world-again");
    assert_eq!(slugify("already-kebab"), "already-kebab");
}

#[test]
fn slugify_falls_back_when_empty() {
    assert_eq!(slugify("!!!"), "memory");
    assert_eq!(slugify(""), "memory");
}

#[test]
fn file_name_and_summary_derive_from_fields() {
    let m = sample();
    assert_eq!(m.file_name(), "user-prefers-tabs.md");
    let s = m.summary();
    assert_eq!(s.name, "user-prefers-tabs");
    assert_eq!(s.title, "ユーザーはタブを好む");
    assert_eq!(s.kind, MemoryType::User);
    assert_eq!(s.related, vec!["editor-config".to_string()]);
    assert_eq!(s.file, "user-prefers-tabs.md");
}

#[test]
fn markdown_round_trips() {
    let m = sample();
    let text = m.to_markdown();
    assert!(text.starts_with("---\nname: user-prefers-tabs\n"));
    assert!(text.contains("type: user\n"));
    assert!(text.contains("related: [editor-config]\n"));
    assert_eq!(Memory::from_markdown(&text).unwrap(), m);
}

#[test]
fn from_markdown_accepts_crlf_and_empty_related() {
    let text = "---\r\nname: n\r\ntitle: t\r\ntype: project\r\nrelated: []\r\n\
        created_at: 2026-06-17T00:00:00Z\r\nupdated_at: 2026-06-17T00:00:00Z\r\n---\r\n\r\nbody\r\n";
    let m = Memory::from_markdown(text).unwrap();
    assert_eq!(m.name, "n");
    assert!(m.related.is_empty());
    assert_eq!(m.kind, MemoryType::Project);
    assert_eq!(m.body, "body");
}

#[test]
fn from_markdown_skips_blank_frontmatter_lines() {
    // A blank line inside the frontmatter block is tolerated (skipped).
    let text = "---\nname: n\n\ntitle: t\ntype: user\n\
        created_at: 2026-06-17T00:00:00Z\nupdated_at: 2026-06-17T00:00:00Z\n---\n\nbody\n";
    let m = Memory::from_markdown(text).unwrap();
    assert_eq!(m.name, "n");
    assert_eq!(m.title, "t");
}

#[test]
fn from_markdown_ignores_unknown_keys() {
    let text = "---\nname: n\ntitle: t\ntype: reference\nrelated: []\nextra: ignored\n\
        created_at: 2026-06-17T00:00:00Z\nupdated_at: 2026-06-17T00:00:00Z\n---\n\nbody\n";
    let m = Memory::from_markdown(text).unwrap();
    assert_eq!(m.kind, MemoryType::Reference);
}

#[test]
fn from_markdown_requires_opening_fence() {
    let err = Memory::from_markdown("no frontmatter").unwrap_err();
    assert!(err.to_string().contains("opening"));
}

#[test]
fn from_markdown_requires_closing_fence() {
    let err = Memory::from_markdown("---\nname: n\n").unwrap_err();
    assert!(err.to_string().contains("closing"));
}

#[test]
fn from_markdown_rejects_a_line_without_colon() {
    let err = Memory::from_markdown("---\nbogus line\n---\n").unwrap_err();
    assert!(err.to_string().contains("invalid frontmatter line"));
}

#[test]
fn from_markdown_reports_missing_required_fields() {
    let base = [
        "name: n",
        "title: t",
        "type: user",
        "created_at: 2026-06-17T00:00:00Z",
        "updated_at: 2026-06-17T00:00:00Z",
    ];
    for (skip, needle) in [
        (0, "missing 'name'"),
        (1, "missing 'title'"),
        (3, "missing 'created_at'"),
        (4, "missing 'updated_at'"),
    ] {
        let kept: Vec<&str> = base
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, l)| *l)
            .collect();
        let text = format!("---\n{}\n---\n\nbody\n", kept.join("\n"));
        let err = Memory::from_markdown(&text).unwrap_err();
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
}

#[test]
fn from_markdown_rejects_invalid_timestamp() {
    let text = "---\nname: n\ntitle: t\ntype: user\ncreated_at: nope\n\
        updated_at: 2026-06-17T00:00:00Z\n---\n\nbody\n";
    let err = Memory::from_markdown(text).unwrap_err();
    assert!(err.to_string().contains("invalid timestamp"));
}

#[test]
fn parse_error_displays_its_message() {
    let err = ParseMemoryError("boom".to_string());
    assert_eq!(err.to_string(), "boom");
}

#[test]
fn related_with_special_characters_round_trips_losslessly() {
    let mut memory = sample();
    // Each entry exercises a structural character of the `[a, b, c]` encoding.
    memory.related = vec![
        "a, b".to_string(),
        "[bracketed]".to_string(),
        "back\\slash".to_string(),
        "  spaced  ".to_string(),
        "plain".to_string(),
    ];
    let text = memory.to_markdown();
    let parsed = Memory::from_markdown(&text).unwrap();
    assert_eq!(parsed.related, memory.related);
}

#[test]
fn related_with_a_comma_is_one_value_not_two() {
    // Regression: `a, b` used to split into `["a", "b"]` on reload.
    let mut memory = sample();
    memory.related = vec!["a, b".to_string()];
    let parsed = Memory::from_markdown(&memory.to_markdown()).unwrap();
    assert_eq!(parsed.related, vec!["a, b".to_string()]);
}

#[test]
fn simple_related_renders_unescaped_and_still_parses() {
    // Plain values carry no escapes, so the on-disk shape stays readable and
    // hand-written / legacy files keep parsing.
    let mut memory = sample();
    memory.related = vec!["editor-config".to_string(), "tabs".to_string()];
    let text = memory.to_markdown();
    assert!(text.contains("related: [editor-config, tabs]\n"));
    assert_eq!(
        Memory::from_markdown(&text).unwrap().related,
        vec!["editor-config".to_string(), "tabs".to_string()]
    );
}

#[test]
fn empty_related_list_round_trips() {
    let mut memory = sample();
    memory.related.clear();
    let text = memory.to_markdown();
    assert!(text.contains("related: []\n"));
    assert!(Memory::from_markdown(&text).unwrap().related.is_empty());
}

#[test]
fn parse_keeps_a_stray_backslash_and_decodes_an_escaped_comma() {
    // Hand-authored frontmatter: `c:\path` carries a backslash before a
    // non-escapable char (kept verbatim), and `a\, b` is one comma-bearing item.
    let text = "---\nname: n\ntitle: t\ntype: project\n\
        related: [c:\\path, a\\, b]\ncreated_at: 2026-06-17T00:00:00Z\n\
        updated_at: 2026-06-17T00:00:00Z\n---\n\nbody\n";
    let m = Memory::from_markdown(text).unwrap();
    assert_eq!(m.related, vec!["c:\\path".to_string(), "a, b".to_string()]);
}

#[test]
fn to_markdown_neutralises_newlines_so_values_cannot_inject_frontmatter() {
    let mut memory = sample();
    // A title that, written verbatim, would forge a `type` frontmatter line.
    memory.title = "メモ\ntype: reference".to_string();

    let md = memory.to_markdown();
    assert!(md.contains("title: メモ type: reference"));

    // Reloads cleanly and the forged `type` never took effect.
    let parsed = Memory::from_markdown(&md).unwrap();
    assert_eq!(parsed.kind, memory.kind); // not overwritten to `reference`
    assert_eq!(parsed.title, "メモ type: reference");
}
