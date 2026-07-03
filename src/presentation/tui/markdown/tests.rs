use super::*;

/// The styles of a line's spans, for terse assertions.
fn styles(line: &MarkdownLine) -> Vec<SpanStyle> {
    line.spans.iter().map(|s| s.style).collect()
}

#[test]
fn blank_lines_become_empty_text_lines() {
    let lines = render("\n");
    // "a\nb".split('\n') yields two parts; "\n" yields ["", ""].
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line.style, LineStyle::Text);
        assert!(line.spans.is_empty());
        assert!(line.prefix.is_empty());
    }
}

#[test]
fn headings_capture_their_level_and_text() {
    for (src, level) in [
        ("# One", 1u8),
        ("## Two", 2),
        ("### Three", 3),
        ("###### Six", 6),
    ] {
        let line = &render(src)[0];
        assert_eq!(line.style, LineStyle::Heading(level));
        assert_eq!(line.plain_text(), src.trim_start_matches('#').trim());
    }
}

#[test]
fn seven_hashes_is_not_a_heading() {
    let line = &render("####### too deep")[0];
    assert_eq!(line.style, LineStyle::Text);
}

#[test]
fn a_hash_without_a_space_is_not_a_heading() {
    let line = &render("#tag")[0];
    assert_eq!(line.style, LineStyle::Text);
    assert_eq!(line.plain_text(), "#tag");
}

#[test]
fn a_bare_hash_line_is_an_empty_heading() {
    let line = &render("#")[0];
    assert_eq!(line.style, LineStyle::Heading(1));
    assert!(line.spans.is_empty());
}

#[test]
fn unordered_list_markers_are_recognised() {
    for marker in ['-', '*', '+'] {
        let line = &render(&format!("{marker} item"))[0];
        assert_eq!(line.style, LineStyle::Bullet);
        assert_eq!(line.prefix, "• ");
        assert_eq!(line.plain_text(), "item");
    }
}

#[test]
fn nested_list_items_keep_their_indentation_in_the_prefix() {
    let line = &render("  - nested")[0];
    assert_eq!(line.style, LineStyle::Bullet);
    assert_eq!(line.prefix, "  • ");
    assert_eq!(line.plain_text(), "nested");
}

#[test]
fn ordered_list_markers_keep_their_number() {
    let dot = &render("1. first")[0];
    assert_eq!(dot.style, LineStyle::Number);
    assert_eq!(dot.prefix, "1. ");
    assert_eq!(dot.plain_text(), "first");

    let paren = &render("12) twelfth")[0];
    assert_eq!(paren.style, LineStyle::Number);
    assert_eq!(paren.prefix, "12. ");
    assert_eq!(paren.plain_text(), "twelfth");
}

#[test]
fn a_number_without_a_following_marker_is_plain_text() {
    let line = &render("1234 things")[0];
    assert_eq!(line.style, LineStyle::Text);
}

#[test]
fn block_quotes_drop_the_marker_and_one_space() {
    let spaced = &render("> quoted")[0];
    assert_eq!(spaced.style, LineStyle::Quote);
    assert_eq!(spaced.prefix, "│ ");
    assert_eq!(spaced.plain_text(), "quoted");

    // A quote with no space after `>` still strips just the marker.
    let tight = &render(">tight")[0];
    assert_eq!(tight.style, LineStyle::Quote);
    assert_eq!(tight.plain_text(), "tight");
}

#[test]
fn fenced_code_blocks_highlight_without_inline_parsing() {
    let src = "```rust\nlet x = `not code`;\n```";
    let lines = render(src);
    // The two fence lines are dropped; only the body line remains.
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style, LineStyle::Code);
    // Highlighting splits the line into several spans, every one a code span
    // (the inline backticks are not parsed), and the text is preserved verbatim.
    assert!(lines[0].spans.len() > 1);
    assert!(lines[0].spans.iter().all(|s| s.style == SpanStyle::Code));
    assert_eq!(lines[0].plain_text(), "let x = `not code`;");
}

#[test]
fn highlighted_code_spans_carry_a_foreground_colour() {
    let lines = render("```rust\nfn main() {}\n```");
    assert!(lines[0].spans.iter().all(|s| s.color.is_some()));
}

#[test]
fn tilde_fences_also_delimit_code_blocks() {
    let src = "~~~\ncode\n~~~";
    let lines = render(src);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style, LineStyle::Code);
    assert_eq!(lines[0].plain_text(), "code");
}

#[test]
fn unknown_language_falls_back_to_plain_text() {
    // An unrecognised info string still renders its body, one line per source
    // line, without panicking.
    let lines = render("```nonexistent-lang\nplain body\n```");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style, LineStyle::Code);
    assert_eq!(lines[0].plain_text(), "plain body");
}

#[test]
fn tabs_in_code_lines_are_expanded_to_spaces() {
    // A tab in a code line would measure as zero width and overrun the pane;
    // it is expanded to spaces so the rendered text lines up. The body has no
    // tab character left in it.
    let lines = render("```\n\tindented\n```");
    assert_eq!(lines.len(), 1);
    let text = lines[0].plain_text();
    assert!(!text.contains('\t'), "tab survived: {text:?}");
    assert!(text.starts_with("    indented"));
}

#[test]
fn a_language_alias_still_highlights() {
    // `sh` is not a registered syntax token but resolves to bash via the alias
    // map, so the body still highlights (several coloured spans) instead of
    // falling back to one plain span.
    let lines = render("```sh\necho hi # note\n```");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style, LineStyle::Code);
    assert_eq!(lines[0].plain_text(), "echo hi # note");
    assert!(
        lines[0].spans.len() > 1,
        "alias did not resolve to a highlighting syntax: {:?}",
        lines[0].spans
    );
}

#[test]
fn an_enormous_input_is_truncated_to_a_line_cap() {
    // Far more lines than the cap: the result is bounded and ends with a
    // truncation marker rather than growing without limit.
    let src = "x\n".repeat(30_000);
    let lines = render(&src);
    assert!(lines.len() <= 20_001, "not capped: {}", lines.len());
    assert_eq!(lines.last().unwrap().plain_text(), "… (preview truncated)");
}

#[test]
fn fence_info_string_is_parsed_case_insensitively_with_extras() {
    // `Rust,ignore` → language token `rust`; highlighting must still succeed.
    let lines = render("```RUST ignore\nfn main() {}\n```");
    assert_eq!(lines[0].style, LineStyle::Code);
    assert_eq!(lines[0].plain_text(), "fn main() {}");
}

#[test]
fn blank_lines_inside_code_blocks_are_preserved() {
    let lines = render("```rust\nlet a = 1;\n\nlet b = 2;\n```");
    assert_eq!(lines.len(), 3);
    assert!(lines[1].spans.is_empty());
    assert_eq!(lines[0].plain_text(), "let a = 1;");
    assert_eq!(lines[2].plain_text(), "let b = 2;");
}

#[test]
fn unterminated_code_fence_still_renders_its_body() {
    let lines = render("```rust\nfn main() {}");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style, LineStyle::Code);
    assert_eq!(lines[0].plain_text(), "fn main() {}");
}

#[test]
fn inline_code_becomes_a_code_span() {
    let line = &render("use `cargo test` now")[0];
    assert_eq!(
        styles(line),
        vec![SpanStyle::Plain, SpanStyle::Code, SpanStyle::Plain]
    );
    assert_eq!(line.spans[1].text, "cargo test");
}

#[test]
fn an_unclosed_backtick_is_literal() {
    let line = &render("a `lonely backtick")[0];
    assert_eq!(styles(line), vec![SpanStyle::Plain]);
    assert_eq!(line.plain_text(), "a `lonely backtick");
}

#[test]
fn strong_and_emphasis_are_distinguished() {
    let line = &render("**bold** and *italic*")[0];
    assert_eq!(
        styles(line),
        vec![SpanStyle::Strong, SpanStyle::Plain, SpanStyle::Emphasis,]
    );
    assert_eq!(line.spans[0].text, "bold");
    assert_eq!(line.spans[2].text, "italic");
}

#[test]
fn underscores_also_mark_strong_and_emphasis() {
    let line = &render("__b__ _i_")[0];
    assert_eq!(
        styles(line),
        vec![SpanStyle::Strong, SpanStyle::Plain, SpanStyle::Emphasis]
    );
}

#[test]
fn unterminated_emphasis_markers_are_literal() {
    let strong = &render("**not closed")[0];
    assert_eq!(strong.plain_text(), "**not closed");
    assert_eq!(styles(strong), vec![SpanStyle::Plain]);

    let lone = &render("a * b")[0];
    assert_eq!(lone.plain_text(), "a * b");
    assert_eq!(styles(lone), vec![SpanStyle::Plain]);
}

#[test]
fn links_keep_their_text_and_drop_the_url() {
    let line = &render("see [the docs](https://example.com) now")[0];
    assert_eq!(
        styles(line),
        vec![SpanStyle::Plain, SpanStyle::Link, SpanStyle::Plain]
    );
    assert_eq!(line.spans[1].text, "the docs");
    assert_eq!(line.plain_text(), "see the docs now");
}

#[test]
fn a_bracket_without_a_following_paren_is_literal() {
    let line = &render("[just brackets] here")[0];
    assert_eq!(line.plain_text(), "[just brackets] here");
    assert_eq!(styles(line), vec![SpanStyle::Plain]);
}

#[test]
fn an_unclosed_link_paren_is_literal() {
    let line = &render("[label](unclosed")[0];
    assert_eq!(line.plain_text(), "[label](unclosed");
}

#[test]
fn crlf_line_endings_are_tolerated() {
    let lines = render("# Title\r\nbody\r\n");
    assert_eq!(lines[0].style, LineStyle::Heading(1));
    assert_eq!(lines[0].plain_text(), "Title");
    assert_eq!(lines[1].plain_text(), "body");
}

#[test]
fn empty_source_renders_no_lines() {
    assert!(render("").is_empty());
}

#[test]
fn mixed_inline_styles_combine_in_order() {
    let line = &render("a **b** `c` [d](e) *f*")[0];
    assert_eq!(
        styles(line),
        vec![
            SpanStyle::Plain,
            SpanStyle::Strong,
            SpanStyle::Plain,
            SpanStyle::Code,
            SpanStyle::Plain,
            SpanStyle::Link,
            SpanStyle::Plain,
            SpanStyle::Emphasis,
        ]
    );
}
