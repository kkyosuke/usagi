//! session に属する Agent 群の表示語彙（並び順・glyph・色・短縮ラベル・要約）。
//!
//! 同じ事実（1 session の Agent 群と各 phase）を Home sidebar の agent 行と
//! [`super::garden`] の plot が別々の言葉で描くと、同じ画面の 2 か所が食い違う。
//! ここを単一情報源にして、両者は同じ順序・同じ記号・同じ数え方だけを使う。
//!
//! 実 IO を持たない純粋関数で、色は [`crate::presentation::theme`] の役割語彙から取る。

use usagi_core::domain::id::AgentRuntimeId;
use usagi_core::domain::session_lifecycle::AgentPhase;

use crate::presentation::theme::{Role, Style};

use super::{clip_to_width, display_width};

/// 1 session に属する Agent 1 つの表示素材。
///
/// [`runtime_id`](Self::runtime_id) は表示せず、安定した並び順と animation の
/// 種にだけ使う identity である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStatus {
    pub runtime_id: AgentRuntimeId,
    pub phase: AgentPhase,
}

/// 注意を要する順の rank。小さいほど先に見せる。
///
/// `waiting`（人の入力待ち）が最優先で、以降 `running` → `ready` →
/// `interrupted` → `absent` → `done` と、放置してよいものほど後ろへ落ちる。
#[must_use]
pub const fn attention_rank(phase: AgentPhase) -> u8 {
    match phase {
        AgentPhase::Waiting => 0,
        AgentPhase::Running => 1,
        AgentPhase::Ready => 2,
        AgentPhase::Interrupted => 3,
        AgentPhase::Absent => 4,
        AgentPhase::Ended | AgentPhase::Exited => 5,
    }
}

/// [`attention_rank`] と runtime identity で安定に並べ替えた複製を返す。
///
/// 同 rank 内を runtime ID で決めるため、phase が変わらない限り並びは frame を
/// またいで動かない。
#[must_use]
pub fn ordered(agents: &[AgentStatus]) -> Vec<AgentStatus> {
    let mut ordered = agents.to_vec();
    ordered.sort_by_key(|agent| (attention_rank(agent.phase), agent.runtime_id));
    ordered
}

/// phase 1 つを表す 1 桁の記号。Nerd Font ではなく BMP の幾何記号を使う。
#[must_use]
pub const fn glyph(phase: AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Waiting => "◆",
        AgentPhase::Running => "●",
        AgentPhase::Ready => "○",
        AgentPhase::Interrupted => "◌",
        AgentPhase::Absent => "·",
        AgentPhase::Ended | AgentPhase::Exited => "◦",
    }
}

/// phase 1 つの色。役割は [`Role`] で選び、生の色は選ばない。
#[must_use]
pub fn style(phase: AgentPhase) -> Style {
    match phase {
        AgentPhase::Waiting => Role::Warning.style().bold(),
        AgentPhase::Running => Role::Success.style().bold(),
        AgentPhase::Ready => Role::Accent.style(),
        AgentPhase::Interrupted => Role::Warning.style().dim(),
        AgentPhase::Absent | AgentPhase::Ended | AgentPhase::Exited => Style::new().dim(),
    }
}

/// 件数の要約で使う短縮ラベル。狭い sidebar と 28 桁の plot の両方に収める。
#[must_use]
pub const fn short_label(phase: AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Waiting => "wait",
        AgentPhase::Running => "run",
        AgentPhase::Ready => "ready",
        AgentPhase::Interrupted => "int",
        AgentPhase::Absent => "idle",
        AgentPhase::Ended | AgentPhase::Exited => "done",
    }
}

/// 記号列と件数要約の間に置く桁数。要約はここを含めて入るときだけ描く。
const SUMMARY_GAP: usize = 2;

/// 要約に並べる phase の代表と、その順序。
///
/// [`attention_rank`] と同じ順で、`Ended` / `Exited` は `done` の 1 項目に畳む。
const SUMMARY_ORDER: [AgentPhase; 6] = [
    AgentPhase::Waiting,
    AgentPhase::Running,
    AgentPhase::Ready,
    AgentPhase::Interrupted,
    AgentPhase::Absent,
    AgentPhase::Ended,
];

/// 件数要約の項目を注目順に並べたもの。0 件の phase は含めない。
fn summary_parts(agents: &[AgentStatus]) -> Vec<String> {
    SUMMARY_ORDER
        .into_iter()
        .filter_map(|phase| {
            let rank = attention_rank(phase);
            let count = agents
                .iter()
                .filter(|agent| attention_rank(agent.phase) == rank)
                .count();
            (count > 0).then(|| format!("{count} {}", short_label(phase)))
        })
        .collect()
}

/// `1 wait · 2 run` 形式の件数要約（色は付けない）。0 件の phase は省く。
#[must_use]
pub fn summary(agents: &[AgentStatus]) -> String {
    summary_parts(agents).join(" · ")
}

/// `width` 桁に収まる範囲で、Agent 1 つにつき記号 1 つを [`ordered`] の順に描く。
///
/// 収まらなかったぶんは末尾の `+N` に畳む。`+N` すら置けない幅では、置ける桁数まで
/// 切り詰めた `+N` だけを返す。空の入力には空文字列を返し、呼び出し側が
/// 「Agent なし」の表現を選べるようにする。
#[must_use]
pub fn glyph_strip(agents: &[AgentStatus], width: usize) -> String {
    if agents.is_empty() || width == 0 {
        return String::new();
    }
    let ordered = ordered(agents);
    let total = ordered.len();
    // 記号は 1 桁ずつ、間に 1 桁の空白を挟む。溢れたぶんは " +N" に畳む。
    let overflow_cost = |hidden: usize| {
        if hidden == 0 {
            0
        } else {
            display_width(&format!(" +{hidden}"))
        }
    };
    let mut visible = total;
    loop {
        let glyphs = if visible == 0 { 0 } else { visible * 2 - 1 };
        let cost = glyphs + overflow_cost(total - visible);
        if cost <= width || visible == 0 {
            break;
        }
        visible -= 1;
    }
    let hidden = total - visible;
    let painted = ordered[..visible]
        .iter()
        .map(|agent| style(agent.phase).paint(glyph(agent.phase)))
        .collect::<Vec<_>>()
        .join(" ");
    if hidden == 0 {
        return painted;
    }
    let marker = Style::new().dim().paint(&format!("+{hidden}"));
    if visible == 0 {
        return clip_to_width(&marker, width);
    }
    format!("{painted} {marker}")
}

/// `● ● ◆  2 run · 1 wait` 形式の 1 行。sidebar の agent 行と Garden の plot が
/// 共有する、`width` 桁に収めた Agent 群の状態表示である。
///
/// 記号列を優先し、余った桁に 2 桁の間隔を空けて件数要約を置く。要約が入らない幅では
/// 記号列だけを返すので、Agent が何体どの phase かは狭い端末でも失われない。
/// Agent が 0 件なら空文字列を返し、「Agent なし」の表現は呼び出し側が選ぶ。
#[must_use]
pub fn status_line(agents: &[AgentStatus], width: usize) -> String {
    let strip = glyph_strip(agents, width);
    if strip.is_empty() {
        return String::new();
    }
    // 要約は注目順に並んでいるので、入らないときは末尾（注目度の低い phase）から
    // 落とす。全部落とすより「待っている 1 体」だけでも言えるほうが読者の役に立つ。
    let mut parts = summary_parts(agents);
    let budget = width
        .saturating_sub(display_width(&strip))
        .saturating_sub(SUMMARY_GAP);
    while !parts.is_empty() && display_width(&parts.join(" · ")) > budget {
        parts.pop();
    }
    if parts.is_empty() {
        return strip;
    }
    format!(
        "{strip}{}{}",
        " ".repeat(SUMMARY_GAP),
        Style::new().dim().paint(&parts.join(" · "))
    )
}

#[cfg(test)]
mod tests;
