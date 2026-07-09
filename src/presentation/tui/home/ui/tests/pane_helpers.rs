use super::*;

use crate::domain::resource::ResourceUsage;
use crate::domain::settings::{LabelColor, SessionLabelDef, SessionLabelMaster};
use crate::domain::workspace_state::{AheadBehind, DiffStat};
use crate::presentation::theme::Palette;
use chrono::DateTime;
use console::{style, Style};

#[test]
fn selected_row_span_falls_back_when_there_are_no_rows_to_walk() {
    // A degenerate list with no groups has nothing to walk, so the span falls
    // back to a single-line block pinned at the top.
    let list = WorktreeList::from_groups(Vec::new());
    assert_eq!(selected_row_span(&list, true), (0, 1));
}

#[test]
fn resource_inline_label_tinted_carries_the_figures_for_every_load_band() {
    // The CPU and memory fields are tinted by their own load band (dim / yellow
    // / red); whatever the tint, both figures still read through. Cover calm,
    // busy, and hot for each field.
    for usage in [
        ResourceUsage {
            cpu_percent: 1,
            memory_bytes: 1,
        }, // calm / calm
        ResourceUsage {
            cpu_percent: 50,
            memory_bytes: 600 * 1024 * 1024,
        }, // busy / busy
        ResourceUsage {
            cpu_percent: 200,
            memory_bytes: 3 * 1024 * 1024 * 1024,
        }, // hot / hot
    ] {
        let plain = console::strip_ansi_codes(&resource_inline_label_tinted(usage)).into_owned();
        assert!(
            plain.contains(&usage.format_cpu()),
            "{plain:?} keeps the CPU figure"
        );
        assert!(
            plain.contains(&usage.format_memory()),
            "{plain:?} keeps the memory figure"
        );
    }
}

fn label_def(id: &str, name: &str, color: LabelColor, icon: Option<&str>) -> SessionLabelDef {
    SessionLabelDef {
        id: id.to_string(),
        name: name.to_string(),
        color,
        icon: icon.map(str::to_string),
    }
}

fn wt(branch: &str, path: &str) -> WorktreeState {
    WorktreeState {
        branch: Some(branch.to_string()),
        path: PathBuf::from(path),
        head: "abc1234".to_string(),
        primary: false,
        upstream: None,
        status: BranchStatus::Local,
        diff: None,
        ahead_behind: None,
        pr: Vec::new(),
        updated_at: Utc::now(),
    }
}

#[test]
fn label_style_maps_every_colour_to_its_palette_role() {
    // Each colour renders through the semantic palette; cover all arms and pin
    // the mapping to the concrete ANSI colour it should resolve to.
    for (color, expected) in [
        (LabelColor::Gray, Style::new().dim()),
        (LabelColor::Red, Style::new().danger()),
        (LabelColor::Green, Style::new().success()),
        (LabelColor::Yellow, Style::new().warning()),
        (LabelColor::Blue, Style::new().info()),
        (LabelColor::Magenta, Style::new().feature()),
        (LabelColor::Cyan, Style::new().accent()),
    ] {
        assert_eq!(
            label_style(color)
                .force_styling(true)
                .apply_to("x")
                .to_string(),
            expected.force_styling(true).apply_to("x").to_string()
        );
    }
}

#[test]
fn label_cell_renders_the_glyph_and_name_pads_blank_and_drops_at_zero() {
    let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
    // A set label shows its glyph and name, and the cell is exactly `col` wide.
    let cell = label_cell(Some(&def), 10);
    let plain = console::strip_ansi_codes(&cell).into_owned();
    assert!(plain.contains("◇ Review"), "{plain:?} shows the label");
    assert_eq!(console::measure_text_width(&cell), 10);
    // An unset row holds the same width in blanks.
    let blank = label_cell(None, 10);
    assert_eq!(console::measure_text_width(&blank), 10);
    assert!(console::strip_ansi_codes(&blank).trim().is_empty());
    // A zero-width column (no visible label anywhere) draws nothing.
    assert_eq!(label_cell(Some(&def), 0), "");
    assert_eq!(label_cell(None, 0), "");
}

#[test]
fn label_cell_clips_a_long_name_to_the_column() {
    let def = label_def("x", "A very long status name", LabelColor::Gray, None);
    let cell = label_cell(Some(&def), 8);
    // The cell fills its column by plain display width, ellipsis included.
    assert_eq!(console::measure_text_width(&cell), 8);
}

#[test]
fn label_col_width_sizes_to_the_widest_master_label_not_the_visible_one() {
    // Two short labels: the column is sized to the wider of the two in the
    // master, regardless of which one a session actually shows.
    let master = SessionLabelMaster {
        labels: vec![
            label_def("todo", "Todo", LabelColor::Gray, Some("○")),
            label_def("blocked", "Blocked", LabelColor::Gray, Some("✕")),
        ],
    };
    // No labels assigned → the column is dropped (0), leaving the sidebar as it
    // was before the feature.
    let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
    assert_eq!(label_col_width(&list, &master), 0);
    // The narrow `todo` is shown, but the column reserves the master's widest
    // (`✕ Blocked`) + a separating space, so cycling to `blocked` will not
    // resize the column and shift the row.
    let widest = "✕ Blocked".chars().count() + 1;
    list.set_label_ids(vec![Some("todo".to_string())]);
    assert_eq!(label_col_width(&list, &master), widest);
    // Cycling to the wider label keeps the same column width — no shift.
    list.set_label_ids(vec![Some("blocked".to_string())]);
    assert_eq!(label_col_width(&list, &master), widest);
}

#[test]
fn label_col_width_caps_a_long_master_label() {
    // A master label longer than the cap clamps to LABEL_COL_MAX (+1 separator).
    let master = SessionLabelMaster {
        labels: vec![label_def(
            "long",
            "Averylonglabelname",
            LabelColor::Gray,
            Some("◇"),
        )],
    };
    let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
    list.set_label_ids(vec![Some("long".to_string())]);
    assert_eq!(label_col_width(&list, &master), LABEL_COL_MAX + 1);
}

#[test]
fn rail_label_glyph_shows_the_coloured_glyph_or_nothing() {
    let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
    let glyph = rail_label_glyph(Some(&def)).unwrap();
    assert!(console::strip_ansi_codes(&glyph).contains('◇'));
    assert_eq!(rail_label_glyph(None), None);
}

#[test]
fn worktree_row_draws_the_manual_status_label_on_line_one() {
    let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
    let (line1, _) = worktree_row(
        &wt("main", "/r/main"),
        "",
        Some(&def),
        "◇ Review".chars().count() + 1,
        10,
        20,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        true,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(console::strip_ansi_codes(&line1).contains("◇ Review"));
}

#[test]
fn root_row_reserves_the_label_column_without_drawing_a_label() {
    // The root carries no label, but with a label column active its blank cell
    // keeps the right-edge note field aligned with the sessions below.
    let (with_col, _) = root_row(10, 8, 20, false, false, false, false);
    let (without_col, _) = root_row(10, 0, 20, false, false, false, false);
    assert_eq!(
        console::measure_text_width(&with_col),
        console::measure_text_width(&without_col) + 8
    );
}

#[test]
fn left_pane_full_and_rail_draw_a_sessions_manual_label() {
    let master = SessionLabelMaster {
        labels: vec![label_def(
            "review",
            "Review",
            LabelColor::Magenta,
            Some("◇"),
        )],
    };
    let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
    list.set_label_ids(vec![Some("review".to_string())]);
    let empty = HashSet::new();
    let res = HashMap::new();
    let join = |lines: Vec<String>| {
        lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    // The full sidebar spells out the label (glyph + name) beside the session.
    let full = join(left_pane(
        &list,
        &empty,
        &empty,
        &empty,
        &empty,
        &[],
        &res,
        &master,
        60,
        40,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    ));
    assert!(full.contains("◇ Review"), "{full:?}");
    // The collapsed rail shows just the coloured glyph.
    let rail = join(left_pane(
        &list,
        &empty,
        &empty,
        &empty,
        &empty,
        &[],
        &res,
        &master,
        RAIL_WIDTH,
        40,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    ));
    assert!(rail.contains('◇'), "{rail:?}");
}

#[test]
fn name_cell_pads_by_display_width_not_char_count() {
    // The cell pads by *display* columns, not char count: `あ機能` is 3 chars
    // but 6 display columns, so padding to a width-8 cell adds 2 columns (not 5
    // chars) and the cell measures exactly 8 — SGR escapes have zero display
    // width. The old `format!("{:<8}")` padded by chars and overran to 11.
    assert_eq!(
        console::measure_text_width(&name_cell("あ機能", 8, false)),
        8
    );
    // ASCII is unchanged: a short name still pads out to the full width.
    assert_eq!(console::measure_text_width(&name_cell("main", 8, true)), 8);
    // A name already wider than the cell is clipped back to the width.
    assert_eq!(
        console::measure_text_width(&name_cell("あ機能拡張作業", 8, false)),
        8
    );
}

#[test]
fn name_cell_reserves_its_width_for_ambiguous_characters() {
    // A name carrying East Asian *Ambiguous* characters (`→ ① ※`) still fills
    // exactly its cell so the following fixed fields do not shift. usagi's
    // terminals paint these one column wide, which is what
    // [`console::measure_text_width`] counts, so sizing and measuring by it
    // keeps the cell and its neighbours aligned.
    for name in ["feat→x", "review①", "対応※", "→→→→"] {
        assert_eq!(
            console::measure_text_width(&name_cell(name, 10, false)),
            10,
            "cell for {name:?} should reserve exactly its width"
        );
    }
    // Two names of equal *rendered* width — one all-ASCII, one carrying an
    // ambiguous glyph — produce the same cell width, so the fields that butt
    // against the cell land in the same place for both.
    assert_eq!(
        console::measure_text_width(&name_cell("feat→", 10, false)),
        console::measure_text_width(&name_cell("featx", 10, false)),
    );
}

#[test]
fn uncoloured_code_span_falls_back_to_success() {
    // A code-block span with no highlight colour uses the palette's success
    // colour, matching the styling of inline code.
    let span = Span {
        text: "x".to_string(),
        style: SpanStyle::Code,
        color: None,
    };
    assert_eq!(
        styled_span(&span, LineStyle::Code),
        style("x").success().to_string()
    );
}

#[test]
fn coloured_code_span_takes_the_palette_arm_and_keeps_its_text() {
    // A highlighted span goes through the 256-colour arm; its visible text is
    // preserved (colour escapes are stripped when the output is not a TTY).
    let span = Span {
        text: "fn".to_string(),
        style: SpanStyle::Code,
        color: Some(Rgb {
            r: 180,
            g: 120,
            b: 60,
        }),
    };
    let out = styled_span(&span, LineStyle::Code);
    assert_eq!(console::strip_ansi_codes(&out), "fn");
}

#[test]
fn rgb_maps_near_grey_to_the_greyscale_ramp() {
    // Equal channels are grey: they snap into the 232–255 ramp.
    assert!((232..=255).contains(&rgb_to_ansi256(Rgb { r: 0, g: 0, b: 0 })));
    assert!((232..=255).contains(&rgb_to_ansi256(Rgb {
        r: 128,
        g: 130,
        b: 127
    })));
    assert_eq!(
        rgb_to_ansi256(Rgb {
            r: 255,
            g: 255,
            b: 255
        }),
        255
    );
}

#[test]
fn rgb_maps_saturated_colour_to_the_cube() {
    // A clearly chromatic colour lands in the 16–231 colour cube. Pure red
    // is cube index (5,0,0) → 16 + 36*5 = 196.
    assert_eq!(rgb_to_ansi256(Rgb { r: 255, g: 0, b: 0 }), 196);
    let blue = rgb_to_ansi256(Rgb { r: 0, g: 0, b: 255 });
    assert!((16..=231).contains(&blue));
}

#[test]
fn digits_counts_decimal_places_with_a_floor_of_one() {
    assert_eq!(digits(0), 1);
    assert_eq!(digits(9), 1);
    assert_eq!(digits(10), 2);
    assert_eq!(digits(999), 3);
    assert_eq!(digits(1000), 4);
}

#[test]
fn rpad_left_pads_to_the_column_width_and_never_shrinks() {
    assert_eq!(rpad("ab", 5), "   ab");
    // Already at/over width → returned unchanged (rpad never truncates).
    assert_eq!(rpad("abcde", 3), "abcde");
}

#[test]
fn diff_cell_pads_counts_to_fixed_columns_and_blanks_when_absent() {
    // `+N` right-aligned in 3 digit columns, `-M` in 2, so the `+`/`-` of every
    // row line up however many changed lines each session has.
    let cell = diff_cell(
        Some(DiffStat {
            added: 5,
            removed: 3,
        }),
        3,
        2,
    );
    assert_eq!(console::strip_ansi_codes(&cell), "+  5 - 3");
    let wide = diff_cell(
        Some(DiffStat {
            added: 124,
            removed: 18,
        }),
        3,
        2,
    );
    assert_eq!(console::strip_ansi_codes(&wide), "+124 -18");
    // Same width whether or not the row has a diff, so the column never moves.
    assert_eq!(
        console::measure_text_width(&cell),
        console::measure_text_width(&diff_cell(None, 3, 2)),
    );
    assert!(diff_cell(None, 3, 2).trim().is_empty());
}

#[test]
fn commits_cell_aligns_arrows_in_fixed_columns_and_blanks_even_sides() {
    // Both sides drawn in this render (ahead in 2 cols, behind in 1).
    let both = commits_cell(
        Some(AheadBehind {
            ahead: 2,
            behind: 1,
        }),
        2,
        1,
    );
    assert_eq!(console::strip_ansi_codes(&both), "↑ 2 ↓1");
    // This row is even-behind → the `↓` side is blanks, holding the column so
    // the next row's `↓` still lines up.
    let ahead_only = commits_cell(
        Some(AheadBehind {
            ahead: 2,
            behind: 0,
        }),
        2,
        1,
    );
    assert!(console::strip_ansi_codes(&ahead_only).starts_with("↑ 2"));
    // Measured as painted (the `↑` / `↓` arrows are one column wide), the
    // blanked-out side holds exactly the drawn side's width.
    assert_eq!(
        console::measure_text_width(&ahead_only),
        console::measure_text_width(&both),
    );
    // No behind side anywhere in the render → only the `↑` column is spent.
    let no_behind = commits_cell(
        Some(AheadBehind {
            ahead: 3,
            behind: 0,
        }),
        1,
        0,
    );
    assert_eq!(console::strip_ansi_codes(&no_behind), "↑3");
    // No ahead side → only the `↓` column.
    let no_ahead = commits_cell(
        Some(AheadBehind {
            ahead: 0,
            behind: 2,
        }),
        0,
        1,
    );
    assert_eq!(console::strip_ansi_codes(&no_ahead), "↓2");
    // Column dropped entirely → empty.
    assert_eq!(commits_cell(None, 0, 0), "");
    // A drawn column but no measurement for this row → blanks holding the
    // 1-wide arrow slot plus its one digit.
    let none = commits_cell(None, 1, 0);
    assert_eq!(console::measure_text_width(&none), 2);
    assert!(none.trim().is_empty());
}

#[test]
fn detail_cols_widths_reserve_only_the_used_sides() {
    let full = DetailCols {
        time: 8,
        ahead: 2,
        behind: 1,
        added: 3,
        removed: 2,
        pr: 4, // "#123"
    };
    assert_eq!(full.commits_width(), 6); // (1+2) + gap + (1+1), arrows one wide
    assert_eq!(full.badge_width(), 8); // 3 + 2 + 3
    assert_eq!(full.cluster_width(), 8 + 1 + 6 + 1 + 8 + 1 + 4); // four fields, three gaps

    // Only an ahead side, no diff, no time: one field, no gaps, no `↓` columns.
    let ahead_only = DetailCols {
        ahead: 2,
        ..DetailCols::default()
    };
    assert_eq!(ahead_only.commits_width(), 3); // 1-wide arrow + 2 digits
    assert_eq!(ahead_only.badge_width(), 0);
    assert_eq!(ahead_only.cluster_width(), 3);

    // Only a behind side (covers the `up == 0` half of the commit gap).
    let behind_only = DetailCols {
        behind: 2,
        ..DetailCols::default()
    };
    assert_eq!(behind_only.commits_width(), 3); // 1-wide arrow + 2 digits

    assert_eq!(DetailCols::default().cluster_width(), 0);
}

fn at(now: DateTime<Utc>, mins: i64) -> DateTime<Utc> {
    now - chrono::Duration::minutes(mins)
}

#[test]
fn detail_cols_sizes_columns_to_the_widest_visible_session() {
    let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let data = vec![
        (
            at(now, 3),
            Some(DiffStat {
                added: 5,
                removed: 3,
            }),
            Some(AheadBehind {
                ahead: 2,
                behind: 0,
            }),
            pr_width(&[pr(7)]), // "<icon> 1" → 3
        ),
        (
            at(now, 12),
            Some(DiffStat {
                added: 140,
                removed: 8,
            }),
            Some(AheadBehind {
                ahead: 0,
                behind: 13,
            }),
            pr_width(&[pr(412), pr(98)]), // "<icon> 2" → 3
        ),
        // A session with neither a diff nor divergence nor PR: exercises every
        // empty arm so they contribute no columns.
        (at(now, 1), None, None, 0),
    ];
    let cols = detail_cols(&data, now, 9, 60);
    assert_eq!(cols.added, 3); // "140"
    assert_eq!(cols.removed, 1); // "8" / "3"
    assert_eq!(cols.ahead, 1); // "2"
    assert_eq!(cols.behind, 2); // "13"
    assert_eq!(cols.pr, 3); // "<icon> 2" — both sessions fold to one badge
    assert_eq!(
        cols.time,
        console::measure_text_width(&relative_time(now, at(now, 12))) // "12m ago"
    );
}

#[test]
fn detail_cols_reserves_the_pr_slot_even_with_no_pr() {
    let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    // No visible session carries a PR, yet the column holds its reserved width
    // so a session gaining or losing its last PR never shifts the diff beside it.
    let none = detail_cols(
        &[(
            at(now, 3),
            Some(DiffStat {
                added: 1,
                removed: 2,
            }),
            None,
            0,
        )],
        now,
        9,
        60,
    );
    assert_eq!(none.pr, PR_RESERVE_WIDTH);
    // A single-PR badge is exactly the reserve width, so appearing shifts nothing.
    let one = detail_cols(&[(at(now, 3), None, None, pr_width(&[pr(7)]))], now, 9, 60);
    assert_eq!(one.pr, PR_RESERVE_WIDTH);
}

#[test]
fn detail_cols_drops_time_then_commits_under_width_pressure() {
    let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let data = vec![(
        at(now, 3),
        Some(DiffStat {
            added: 1,
            removed: 2,
        }),
        Some(AheadBehind {
            ahead: 2,
            behind: 1,
        }),
        0,
    )];
    // Roomy: every field survives (full cluster needs ~30 columns beside a
    // 9-wide agent label).
    let roomy = detail_cols(&data, now, 9, 60);
    assert!(roomy.time > 0);
    assert!(roomy.ahead > 0 || roomy.behind > 0);
    assert!(roomy.added > 0);
    // Tighter: the lowest-priority time is dropped, commits + badge stay.
    let mid = detail_cols(&data, now, 9, 30);
    assert_eq!(mid.time, 0);
    assert!(mid.ahead > 0 || mid.behind > 0);
    assert!(mid.added > 0);
    // Tightest: commits also dropped, but the badge is always kept.
    let tight = detail_cols(&data, now, 9, 18);
    assert_eq!(tight.time, 0);
    assert_eq!(tight.ahead, 0);
    assert_eq!(tight.behind, 0);
    assert!(tight.added > 0);
}

#[test]
fn detail_content_right_aligns_the_cluster_and_clips_the_agent() {
    let badge = diff_cell(
        Some(DiffStat {
            added: 124,
            removed: 18,
        }),
        3,
        2,
    );
    // Agent label on the left, the cluster pinned to the cell's right edge; the
    // whole cell measures exactly the width so the badges line up.
    let line = detail_content(AgentLifecycle::Running, std::slice::from_ref(&badge), 24);
    assert_eq!(console::measure_text_width(&line), 24);
    let plain = console::strip_ansi_codes(&line);
    // Icon only: the AI glyph + phase icon, no spelled-out word.
    assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
    assert!(!plain.contains("running"));
    assert!(plain.ends_with("+124 -18"));

    // With no agent the cluster still rides the right edge.
    let line = detail_content(AgentLifecycle::Absent, std::slice::from_ref(&badge), 24);
    assert_eq!(console::measure_text_width(&line), 24);
    assert_eq!(console::strip_ansi_codes(&line).trim_start(), "+124 -18");
}

#[test]
fn detail_content_falls_back_to_the_agent_or_clips_a_cramped_cluster() {
    // No cells → just the agent icons (blank when absent, no spelled-out word).
    assert_eq!(detail_content(AgentLifecycle::Absent, &[], 20), "");
    let running = detail_content(AgentLifecycle::Running, &[], 20);
    let running = console::strip_ansi_codes(&running);
    assert!(running.contains('▶'));
    assert!(!running.contains("running"));
    // Cluster alone wider than the cell → clipped to the cell.
    let badge = diff_cell(
        Some(DiffStat {
            added: 124,
            removed: 18,
        }),
        3,
        2,
    );
    let line = detail_content(AgentLifecycle::Running, std::slice::from_ref(&badge), 5);
    assert!(console::measure_text_width(&line) <= 5);
}

#[test]
fn detail_content_joins_the_cells_in_order_with_single_space_gaps() {
    let time = rpad(&style("3m ago").dim().to_string(), 6);
    let commits = commits_cell(
        Some(AheadBehind {
            ahead: 2,
            behind: 1,
        }),
        1,
        1,
    );
    let badge = diff_cell(
        Some(DiffStat {
            added: 1,
            removed: 2,
        }),
        1,
        1,
    );
    let cells = vec![time, commits, badge];
    let line = detail_content(AgentLifecycle::Running, &cells, 40);
    let plain = console::strip_ansi_codes(&line);
    assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
    assert!(plain.contains("3m ago ↑2 ↓1 +1 -2"));
    assert!(plain.ends_with("+1 -2"));
}

#[test]
fn relative_time_buckets_by_elapsed_span() {
    let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let ago = |secs: i64| {
        console::strip_ansi_codes(&relative_time(now, now - chrono::Duration::seconds(secs)))
            .into_owned()
    };
    assert_eq!(ago(5), "now"); // under a minute
    assert_eq!(ago(180), "3m ago"); // minutes
    assert_eq!(ago(7200), "2h ago"); // hours
    assert_eq!(ago(2 * 86_400), "2d ago"); // days
                                           // A future timestamp (clock skew) clamps to "now".
    assert_eq!(
        console::strip_ansi_codes(&relative_time(now, now + chrono::Duration::seconds(30))),
        "now"
    );
}

fn pr(number: u32) -> PrLink {
    PrLink::new(number, format!("https://github.com/o/r/pull/{number}"))
}

#[test]
fn pr_cell_folds_prs_into_an_icon_and_count_and_blanks_when_absent() {
    // One PR rides the right edge of its fixed column as `<icon> 1`; a wider
    // column left-pads with spaces so badges line up down the list.
    let cell = pr_cell(&[pr(7)], 5);
    assert_eq!(console::measure_text_width(&cell), 5);
    assert_eq!(
        console::strip_ansi_codes(&cell),
        format!("  {PR_ICON} 1").as_str()
    );
    // Several PRs fold into one `<icon> <count>` badge, not a `#N #M` run.
    let many = pr_cell(&[pr(412), pr(98)], 3);
    assert_eq!(
        console::strip_ansi_codes(&many),
        format!("{PR_ICON} 2").as_str()
    );
    // No PR fills the same width with blanks, holding the column.
    assert_eq!(pr_cell(&[], 4), "    ");
}

#[test]
fn pr_width_is_the_icon_space_and_count_digits() {
    assert_eq!(pr_width(&[]), 0);
    assert_eq!(pr_width(&[pr(7)]), 3); // "<icon> 1"
    assert_eq!(pr_width(&[pr(412), pr(98)]), 3); // "<icon> 2"
                                                 // A count that reaches two digits widens by one.
    let ten: Vec<PrLink> = (0..10).map(pr).collect();
    assert_eq!(pr_width(&ten), 4); // "<icon> 10"
}

#[test]
fn pr_popup_box_lists_one_pr_per_line_with_its_title() {
    let mut titled = pr(442);
    titled.title = Some("Add PR titles".to_string());
    let popup = pr_popup_box(&[titled, pr(447)]);
    let plain: Vec<String> = popup
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();
    // The top border carries the `PR` title; each PR sits on its own row, with the
    // resolved title beside its number and just `#<n>` while a title is unresolved.
    assert!(plain[0].contains("PR"));
    assert!(plain
        .iter()
        .any(|l| l.contains("#442") && l.contains("Add PR titles")));
    assert!(plain.iter().any(|l| l.contains("#447")));
    // The two PRs are on distinct rows, not packed together on one.
    assert!(!plain
        .iter()
        .any(|l| l.contains("#442") && l.contains("#447")));
    // No PR → no box, so the overlay is a no-op for a session without one.
    assert!(pr_popup_box(&[]).is_empty());
}

#[test]
fn pr_popup_box_keeps_the_title_clear_for_a_single_digit_pr() {
    let popup = pr_popup_box(&[pr(7)]);
    let plain: Vec<String> = popup
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();

    assert_eq!(plain[0], "┌─ PR ┐");
    assert_eq!(plain[1], "│ #7  │");
}

#[test]
fn pr_popup_box_lists_every_pr_on_its_own_row_and_clips_a_long_title() {
    // Each PR is one content row, so twenty PRs make twenty rows plus the two
    // borders — the list stacks vertically rather than packing across a row.
    let many: Vec<PrLink> = (100u32..120).map(pr).collect();
    let popup = pr_popup_box(&many);
    assert_eq!(popup.len(), many.len() + 2);
    // A title wider than the cap is clipped so the box never spans the screen:
    // every row stays within the inner cap plus the two borders and a pad space
    // on each side.
    let mut long = pr(7);
    long.title = Some("x".repeat(200));
    for line in pr_popup_box(std::slice::from_ref(&long)) {
        assert!(console::measure_text_width(&line) <= PR_POPUP_INNER + 4);
    }
}

#[test]
fn detail_content_keeps_the_pr_cell_at_the_right_edge() {
    // The PR cell, as the last in `cells`, lands flush against the right edge
    // beside the diff badge (`+1 -2 <icon> 2`).
    let badge = diff_cell(
        Some(DiffStat {
            added: 1,
            removed: 2,
        }),
        1,
        1,
    );
    let cell = pr_cell(&[pr(412), pr(98)], 3);
    let cells = vec![badge, cell];
    let line = detail_content(AgentLifecycle::Running, &cells, 40);
    let plain = console::strip_ansi_codes(&line);
    assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
    assert!(plain.contains(format!("+1 -2 {PR_ICON} 2").as_str()));
    assert!(plain.ends_with(format!("{PR_ICON} 2").as_str()));
}
