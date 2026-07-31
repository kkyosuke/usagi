---
number: 612
title: fix(tui): Director CLI picker を排他的な前面 input owner にする
status: todo
priority: high
labels: [review, v2, tui, input, terminal, security, correctness]
dependson: []
related: [578, 580, 581, 600]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-07-31T15:00:00+09:00
---

## Finding（P1 TUI）

`crates/tui/src/presentation/mod.rs::handle_workspace_agent_reserved_input` は picker `Choosing` / `Empty` 中も Up/Down/Enter/Escape 等の reserved subset だけを consume し、通常の `Char`、`Paste`、`TerminalCopy`、pane control は `forward_live_terminal_input` や後段 interceptor に落ちる。`WorkspaceRuntime::wants_live_input` は背後の selected live root Agent を引き続き true とし、前面 picker 操作中の paste/文字が PTY に送られ、close/scroll/tab control が背後 state を変更する。

#1371 は同じ module を `director_*` へ rename する open PR だが、入力優先順位は不変と明記しているため修正ではなく競合リスクとして扱う。

## 最小修正方針

foreground input owner を明示した routing gate を最上流に置き、picker active 中は picker が理解しない keyboard/paste/copy/pointer/pane action も inert に consume する。resize/backend tick のみ通す。

## テストと受け入れ条件

- Choosing/Empty で Char、Paste、TerminalCopy、CloseTab、scroll、tab move、pointer を入力しても PTY bytes、pane state、tab stateが不変。
- picker reserved key と cancel/launch は従来どおり動く。
- picker を閉じた直後は同じ ordinary input が focused terminal に届く。
