---
number: 156
title: fix(tui): 没入(Attached)中もキュー済みプロンプトを autostart する
status: done
priority: high
labels: [tui, orchestration]
dependson: []
related: [136, 680]
created_at: 2026-07-09T23:06:22.327373+00:00
updated_at: 2026-07-09T23:19:44.151355+00:00
---

## 背景（症状）

MCP `session_delegate_issue` / `session_prompt` で立てたセッションのキュー済みプロンプトが、コーディネータ agent がペインに没入（Attached）している間は自動起動されず、`Ctrl-O` で選択(Switch)モードに戻って初めて起動する。左ペインには #680 の効果で委譲先セッション行は即出るが、その agent は起動しないままになる。

## 根本原因（実測で確認）

- キュー済みプロンプトの autostart は `autostart_queued_prompts()`（`src/presentation/tui/home/mod.rs`）で行われ、外側 Home イベントループの `apply_autostart`（`src/presentation/tui/home/event/mod.rs`）からしか呼ばれていない。
- 没入(Attached)中は外側ループが止まり、`src/presentation/tui/home/terminal/pane.rs` の `drive()` ループが回る。#680 はここに sidebar 反映のための drain（PR links / sessions_refresh / badges）だけを足したが、autostart は入れていない。
- `open_terminal`（mod.rs）が `let mut pool = pool.borrow_mut();` で `pool` を `terminal::pane::run` を含むループ全体にわたり握りっぱなしのため、pane ループから autostart（`add_pane` に `pool.borrow_mut()` が必要）を単純に呼ぶと RefCell 二重借用でパニックする。

## 方針

`open_terminal` 内の pool 借用を短命化し、没入中の idle tick でも autostart を配線する。

1. `open_terminal` で `pool.borrow_mut()` guard をループ全体で握らず、monitor/tabs/nav/swap_active/add_pane/close_active/snapshot_open_panes_for など各操作を短命な借用に組み替える。`pane::run` には pty の代わりに `&RefCell<TerminalPool>` + `dir` を渡し、pty はループ各周で借用する。
2. `drive()` のループ先頭（前周の pool 借用を drop 済みの borrow-free な地点）で autostart フックを idle tick 相当のスロットルで呼ぶ。フックは外側ループと同じ `autostart_queued_prompts` を呼ぶ closure。適用ロジックは既存のテスト済み `apply_autostart` を共有する。
3. 方針②（一瞬 Switch に抜けて再 attach する PaneExit 変種）は UX が劣るので採らない。

## 受け入れ条件

- コーディネータ agent がペインに没入したまま `session_delegate_issue` / `session_prompt` を実行すると、Switch に戻らなくても委譲先セッションの agent が自動起動する。
- 既存の Switch モードでの autostart 挙動、および現在アタッチ中セッションへの誤 spawn（`has_live_pane` ガード）は退行しない。
- 設定 `autostart_queued_prompts` が OFF のときは従来どおり待機。
