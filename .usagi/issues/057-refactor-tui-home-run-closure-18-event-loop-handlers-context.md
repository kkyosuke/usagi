---
number: 57
title: refactor(tui-home): run の closure 配線整理（18 引数 event_loop・handlers の context 構造体化）
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-06-19T22:16:00.490934+00:00
updated_at: 2026-06-19T22:16:00.490934+00:00
---

## 背景

home の起動・イベント配線が引数過多で seam が広い。

### 1. `mod.rs` の run が 440 行の closure 配線塊（`src/presentation/tui/home/mod.rs`、613 行）
`run` がほぼ全行 closure 配線で、18 引数の `event_loop`（`:479-498`、`#[allow(clippy::too_many_arguments)]`）へ流す。テスト用に `event_loop_compat`（`event/mod.rs:418-491`）が全引数を再スレッドしており、seam が広すぎる兆候。→ `HomeWiring` builder + context 構造体で引数束を 1 つにまとめる。

### 2. handlers が各々 9〜12 引数（`src/presentation/tui/home/event/handlers.rs`、698 行）
`overview_key`/`switch_key`/`focus_key`/`focus_menu_key`/`focus_prompt_key` がいずれも `#[allow(too_many_arguments)]` で `(term, reader, state, painter, workspace_root, ..., open_terminal, preview)` の同じ束を受ける。→ context 構造体に括り出す。あわせて Overview/focus/focus-prompt の 3 箇所で open-coded な「enter focus → open_pane」分岐（`:84-97`）を共通化する。

### 3. `terminal_pane.rs` の `is_*_tab` チョード matcher 重複（`:639-699`）
7 個のほぼ同型 matcher を `chord(key, raw, letter)` ヘルパ 1 つに畳む。

## 確認方法

- `#[allow(clippy::too_many_arguments)]` を外せること。
- 既存テストが通ること（カバレッジ 100% 維持）。
