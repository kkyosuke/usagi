use usagi_core::domain::id::AgentRuntimeId;
use usagi_core::domain::session_lifecycle::AgentPhase;

use super::{
    AgentStatus, attention_rank, glyph, glyph_strip, ordered, short_label, status_line, style,
    summary_parts,
};
use crate::presentation::widgets::display_width;

fn agent(id: &str, phase: AgentPhase) -> AgentStatus {
    AgentStatus {
        runtime_id: AgentRuntimeId::parse(id).expect("valid runtime id"),
        phase,
    }
}

/// 件数要約を 1 本の文字列として読む。production では [`status_line`] が幅に合わせて
/// 末尾から項目を落とすため、要約そのものは項目の列として組み立てられる。
fn summary(agents: &[AgentStatus]) -> String {
    summary_parts(agents).join(" · ")
}

fn runtime(index: u8, phase: AgentPhase) -> AgentStatus {
    agent(&format!("{index:08x}-0000-4000-8000-000000000000"), phase)
}

/// ANSI を落として表示テキストだけを見る。色は役割語彙のテストで別に押さえる。
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for escaped in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&escaped) && escaped != '[' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn attention_rank_puts_waiting_first_and_finished_work_last() {
    let ranks = [
        AgentPhase::Waiting,
        AgentPhase::Running,
        AgentPhase::Ready,
        AgentPhase::Interrupted,
        AgentPhase::Absent,
        AgentPhase::Ended,
    ]
    .map(attention_rank);
    assert_eq!(ranks, [0, 1, 2, 3, 4, 5]);
    // Ended と Exited はどちらも「終わった」1 つの class に畳む。
    assert_eq!(
        attention_rank(AgentPhase::Exited),
        attention_rank(AgentPhase::Ended)
    );
}

#[test]
fn ordering_is_attention_then_runtime_identity() {
    let waiting = runtime(9, AgentPhase::Waiting);
    let early_running = runtime(1, AgentPhase::Running);
    let late_running = runtime(2, AgentPhase::Running);
    let done = runtime(0, AgentPhase::Exited);
    let shuffled = [done, late_running, waiting, early_running];
    assert_eq!(
        ordered(&shuffled),
        vec![waiting, early_running, late_running, done]
    );
    // 入力順が変わっても並びは同じ（frame をまたいで記号が踊らない）。
    let mut reversed = shuffled;
    reversed.reverse();
    assert_eq!(ordered(&reversed), ordered(&shuffled));
}

/// 閉じた phase 語彙のすべてに、1 桁の記号・短いラベル・色を用意する。1 つでも
/// 抜けると、その phase の Agent だけ列がずれるか無色で出る。
#[test]
fn every_phase_has_a_single_column_glyph_a_short_label_and_a_colour() {
    for phase in AgentPhase::ALL {
        assert_eq!(
            display_width(glyph(phase)),
            1,
            "{phase:?} の記号は 1 桁でなければ列がずれる"
        );
        let label = short_label(phase);
        assert!(!label.is_empty());
        assert!(display_width(label) <= 5, "{phase:?} の label が長すぎる");
        // 色を載せても表示桁は変わらない（ANSI は桁を持たない）。
        let painted = style(phase).paint(glyph(phase));
        assert_eq!(display_width(&painted), 1, "{phase:?}");
    }
}

#[test]
fn waiting_and_running_are_the_only_emphasised_phases() {
    // 注意を引く色は「人の入力待ち」と「実行中」だけに割り当て、残りは沈める。
    let painted = |phase| style(phase).paint("x");
    assert_ne!(painted(AgentPhase::Waiting), painted(AgentPhase::Running));
    assert_ne!(painted(AgentPhase::Ready), painted(AgentPhase::Absent));
    assert_eq!(painted(AgentPhase::Absent), painted(AgentPhase::Ended));
    assert_eq!(painted(AgentPhase::Ended), painted(AgentPhase::Exited));
}

#[test]
fn summary_counts_each_phase_class_in_attention_order() {
    let agents = [
        runtime(1, AgentPhase::Ended),
        runtime(2, AgentPhase::Running),
        runtime(3, AgentPhase::Waiting),
        runtime(4, AgentPhase::Exited),
        runtime(5, AgentPhase::Running),
    ];
    assert_eq!(summary(&agents), "1 wait · 2 run · 2 done");
}

#[test]
fn summary_of_no_agents_is_empty() {
    assert_eq!(summary(&[]), String::new());
}

#[test]
fn summary_lists_the_quiet_phases_too() {
    let agents = [
        runtime(1, AgentPhase::Ready),
        runtime(2, AgentPhase::Interrupted),
        runtime(3, AgentPhase::Absent),
    ];
    assert_eq!(summary(&agents), "1 ready · 1 int · 1 idle");
}

#[test]
fn the_glyph_strip_draws_one_marker_per_agent_in_attention_order() {
    let agents = [
        runtime(1, AgentPhase::Ended),
        runtime(2, AgentPhase::Waiting),
        runtime(3, AgentPhase::Running),
    ];
    let strip = glyph_strip(&agents, 20);
    assert_eq!(
        plain(&strip),
        format!(
            "{} {} {}",
            glyph(AgentPhase::Waiting),
            glyph(AgentPhase::Running),
            glyph(AgentPhase::Ended)
        )
    );
}

#[test]
fn the_glyph_strip_folds_agents_it_cannot_fit_into_a_trailing_count() {
    let agents: Vec<_> = (0..6).map(|i| runtime(i, AgentPhase::Running)).collect();
    // 記号 2 つ（3 桁）と ` +4`（3 桁）で 6 桁ちょうど。
    let strip = glyph_strip(&agents, 6);
    assert_eq!(plain(&strip), "● ● +4");
    assert_eq!(display_width(&strip), 6);
}

#[test]
fn a_width_that_fits_no_glyph_still_reports_the_hidden_count() {
    let agents: Vec<_> = (0..3).map(|i| runtime(i, AgentPhase::Running)).collect();
    let strip = glyph_strip(&agents, 2);
    assert_eq!(plain(&strip), "+3");
    assert!(display_width(&strip) <= 2);
}

#[test]
fn a_width_too_narrow_even_for_the_count_clips_it() {
    let agents: Vec<_> = (0..12).map(|i| runtime(i, AgentPhase::Running)).collect();
    let strip = glyph_strip(&agents, 1);
    assert!(display_width(&strip) <= 1);
}

#[test]
fn an_empty_agent_list_draws_nothing() {
    assert_eq!(glyph_strip(&[], 20), String::new());
    assert_eq!(status_line(&[], 20), String::new());
    // 幅 0 でも panic せず、何も描かない。
    assert_eq!(
        glyph_strip(&[runtime(1, AgentPhase::Running)], 0),
        String::new()
    );
}

#[test]
fn the_status_line_appends_the_summary_when_it_fits() {
    let agents = [
        runtime(1, AgentPhase::Waiting),
        runtime(2, AgentPhase::Running),
    ];
    let line = status_line(&agents, 30);
    assert_eq!(plain(&line), "◆ ●  1 wait · 1 run");
    assert!(display_width(&line) <= 30);
}

#[test]
fn the_status_line_drops_the_least_urgent_counts_before_the_whole_summary() {
    let agents = [
        runtime(1, AgentPhase::Waiting),
        runtime(2, AgentPhase::Running),
        runtime(3, AgentPhase::Ready),
        runtime(4, AgentPhase::Ended),
    ];
    assert_eq!(summary(&agents), "1 wait · 1 run · 1 ready · 1 done");
    // 全部は入らない幅では、末尾（注目度の低い phase）から落として先頭を残す。
    assert_eq!(plain(&status_line(&agents, 26)), "◆ ● ○ ◦  1 wait · 1 run");
    assert_eq!(plain(&status_line(&agents, 18)), "◆ ● ○ ◦  1 wait");
}

#[test]
fn the_status_line_keeps_the_glyphs_and_drops_the_summary_when_narrow() {
    let agents = [
        runtime(1, AgentPhase::Waiting),
        runtime(2, AgentPhase::Running),
    ];
    let line = status_line(&agents, 8);
    assert_eq!(plain(&line), "◆ ●");
}

#[test]
fn the_status_line_never_exceeds_the_width_it_is_given() {
    let agents: Vec<_> = (0..9)
        .map(|i| {
            runtime(
                i,
                match i % 3 {
                    0 => AgentPhase::Waiting,
                    1 => AgentPhase::Running,
                    _ => AgentPhase::Ready,
                },
            )
        })
        .collect();
    for width in 0..40 {
        assert!(
            display_width(&status_line(&agents, width)) <= width,
            "width {width} を超えた"
        );
    }
}
