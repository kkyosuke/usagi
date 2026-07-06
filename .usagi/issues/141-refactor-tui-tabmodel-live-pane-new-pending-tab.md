---
number: 141
title: refactor(tui): TabModel を導入し live pane と +new/pending tab の状態遷移を純粋化する
status: todo
priority: high
labels: [refactor, tui, terminal, review]
dependson: [138]
related: [128, 129]
parent: 137
created_at: 2026-07-06T00:20:19.160346+00:00
updated_at: 2026-07-06T00:20:19.160346+00:00
---

## 目的

terminal tab / Focus の `+ new` 仮想タブ / pending spawn を、PTY IO から独立した純粋な `TabModel` として表現する。

## 背景

`terminal/tabs.rs` には `resolve_nav` / `resolve_swap` / `resolve_move` / `active_after_close` などの良い純粋関数がある。一方で実際の状態は `TerminalPool::SessionPanes`、`HomeState::focus_new_tab`、`HomeState::focus_action_over_pane`、pending pane state、tab menu overlay に分かれており、`Ctrl-P/N`、drag/drop、close、rename、background launch、`+ new` の関係が複数ファイルに散っている。

## 変更方針

- `terminal/tabs.rs` を拡張するか、新規 `terminal/tab_model.rs` を作る。
- `TabModel` に以下を持たせる。
  - live pane ids / kind / cli / label override
  - active live pane index
  - virtual `+ new` selected flag
  - pending tab id / label / visual state（必要なら）
- 操作は純粋な reducer にする。
  - `next` / `prev` / `to_live(index)` / `to_new`
  - `move_tab` / `swap` / `rename` / `close`
  - `begin_pending` / `pending_ready` / `pending_gone` / `activate_pending`
- 既存 `PaneTab` / `TabStrip` / `TabNav` / `TabSwap` をできるだけ活かし、PTY 所有型は入れない。

## 対象ファイル

- `src/presentation/tui/home/terminal/tabs.rs`
- `src/presentation/tui/home/terminal/pool.rs`
- `src/presentation/tui/home/state/mod.rs`
- `src/presentation/tui/home/state/modal.rs`
- `src/presentation/tui/home/event/tests/background_tab.rs`
- `src/presentation/tui/home/state/tests/focus.rs`
- `src/presentation/tui/home/state/tests/caret_switch.rs`

## 受け入れ条件

- live tab と `+ new` 仮想タブの遷移が IO なしで単体テストできる。
- 既存 `terminal/tabs.rs` のテストを拡張し、Focus 側の tab 遷移も同じ model で検証できる。
- `HomeState` の `focus_tab_next` / `focus_tab_prev` / `focus_select_pane_tab` の重複ロジックが減る。
- #138 の characterization test が通る。

## テスト方針

- `cargo test terminal::tabs`
- `cargo test background_tab`
- `cargo test state::tests::focus`
- pending spawn / reused agent / last tab close の model-level test を追加する。

## 非目標

- `TerminalPool` から PTY ownership や watcher をこの issue で分離しない。
- tab UI の見た目やマウス hit-test は変更しない。
- shell process の起動・終了挙動は変更しない。
