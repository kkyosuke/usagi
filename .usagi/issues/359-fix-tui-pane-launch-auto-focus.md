---
number: 359
title: fix(tui): pane launch 完了時の auto-focus を「起動後に操作が無いとき」だけに戻す退行修正
status: in-progress
priority: medium
labels: [tui, controller, regression]
dependson: []
related: [315, 316, 258, 358]
created_at: 2026-07-19T11:37:01.491474+00:00
updated_at: 2026-07-19T11:37:01.491474+00:00
---

## 背景

#315 / #316 の controller 移行で、旧経路（#1028 = 08917912）の「loading 中に操作すると完了時の
auto-focus を解除する」契約が controller 経路へ復元されず消失した。

## 退行（R2）

pane launch（agent / terminal）の completion 時に、`crates/tui/src/presentation/mod.rs` の
`drain_pane_completions_into_runtime` が `runtime.complete_pane(...)` に続けて
`runtime.focus_terminal(...)` を**無条件**で呼ぶ。ユーザーが別の画面を読んでいても、完了 tab へ
focus が奪われる。

旧経路には対応テスト
（`pending_tab_is_listed_with_a_wave_and_focuses_only_when_completion_is_uninterrupted` /
`later_interaction_cancels_pending_tab_completion_focus` /
`input_while_an_agent_tab_loads_cancels_its_automatic_focus`）があったが、現ツリーには相当が無い。

## 契約（復元）

controller には create-session 用の interaction gate
（`AppState.interaction_count` / `PendingOperation.interaction_at_accept`）が既にある。同じ発想で
pane launch にも「受付時 interaction」を記録し、completion 時に interaction count が一致した
（＝起動後にユーザー操作が無かった）場合だけ focus する。判断ロジックは runtime 側に置き、shell
（`drain_pane_completions_into_runtime`）には条件分岐を漏らさない。

## 完了条件

- `WorkspaceRuntime` が pane launch 受付時（`request_pane`）に `AppState::interaction_count` を記録し、
  completion 時に一致したときだけ focus する単一の入口（`complete_pane_focus_if_uninterrupted`）を持つ。
  shell は無条件の `focus_terminal` 呼び出しをやめ、その入口だけを呼ぶ。fail / close で gate を破棄する。
- 旧 #1028 のテスト 3 本に相当する回帰テストを controller 経路で復元する
  （無操作→focus、操作あり→focus 解除だが tab は live 昇格）。
- 仕様ドキュメント（`document/03-tui.md` の pane completion focus）を実態に整合させる。
- full test + coverage 100% を通す。
