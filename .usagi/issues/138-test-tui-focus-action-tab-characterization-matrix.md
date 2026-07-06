---
number: 138
title: test(tui): Focus action と tab 操作の現状仕様を characterization matrix で固定する
status: todo
priority: high
labels: [test, tui, review]
dependson: []
related: []
parent: 137
created_at: 2026-07-06T00:19:39.561850+00:00
updated_at: 2026-07-06T00:19:39.561850+00:00
---

## 目的

Focus action / prompt / menu と tab 操作の現状仕様を、リファクタ前に small matrix test として固定する。

## 背景

`focus_menu.rs` は約 1.2k 行、`focus_prompt.rs` は約 275 行、`background_tab.rs` や `state/tests/focus.rs` も個別シナリオを厚く持っている。ただし仕様が「どの action が menu に出るか」「root row では何が隠れるか」「`+ new` / pane tab / action overlay の Esc がどう動くか」「Focus の `Ctrl-P/N` と TerminalPool の active tab がどう同期するか」という表として見えないため、抽出 PR で何を守ればよいかが分かりにくい。

## 変更方針

- 既存挙動を変えず、テストだけ追加・整理する。
- Focus action の matrix を作る。
  - root row / session row
  - Menu / Prompt
  - idle / live pane / action overlay
  - `terminal` / `terminal new` / `agent` / `agent <cli>` / `ai <prompt>` / `chat` / `diff` / `close` / `close --force`
- Tab 操作の matrix を作る。
  - next / prev / To(index)
  - active pane / `+ new` 仮想タブ
  - pending spawn / reused agent / close / rename / move
- 既存の helper を使い、巨大な e2e を増やさず、後続 issue で reducer 単体テストへ移しやすい assertion にする。

## 対象ファイル

- `src/presentation/tui/home/event/tests/focus_menu.rs`
- `src/presentation/tui/home/event/tests/focus_prompt.rs`
- `src/presentation/tui/home/event/tests/background_tab.rs`
- `src/presentation/tui/home/state/tests/focus.rs`
- `src/presentation/tui/home/state/tests/caret_switch.rs`

## 受け入れ条件

- 後続リファクタが参照できる Focus action / tab 操作の matrix test が存在する。
- 既存挙動は変更しない。
- 新規テスト名が仕様を説明しており、失敗時にどの行動が壊れたか分かる。
- この issue 単独で ready にできる（新設計型には依存しない）。

## テスト方針

- `cargo test focus_menu`
- `cargo test focus_prompt`
- `cargo test background_tab`
- 必要に応じて `cargo test state::tests::focus` 相当の絞り込み。

## 非目標

- 実装ファイルのリファクタは行わない。
- UI 表示文言やキー割り当てを変更しない。
- `TerminalPool` の PTY 起動や watcher の実 IO テストは追加しない。
