---
number: 140
title: refactor(tui): SessionAction 定義表を導入し Focus menu の表示条件を data-driven 化する
status: done
priority: high
labels: [refactor, tui, review]
dependson: [138]
related: [45]
parent: 137
created_at: 2026-07-06T00:20:19.113899+00:00
updated_at: 2026-07-09T00:12:13.118813+00:00
---

## 目的

Focus menu が表示する session action を `SessionActionSpec` の定義表に切り出し、表示条件・shortcut・sub-picker を handler から分離する。

## 背景

現在は `command/builtins.rs` の Session command、`HomeState::focus_menu_commands` の filter、`HomeState::focus_menu_*_can_expand`、`FocusMenu` の sub cursor、`handlers.rs::focus_menu_key` / `run_focus_command` が同じ action 語彙を別々に知っている。`agent` / `terminal` / `close` の picker は `FocusSubmenu` と handler の match に直書きされ、`chat` や `diff` の root row 可否も state 側に埋め込まれている。

## 変更方針

- 新規の小モジュール（例: `src/presentation/tui/home/action.rs` または `focus_action.rs`）を追加する。
- `SessionActionSpec` に以下を持たせる。
  - command name / label / description
  - menu に出すか（`ai` のような prompt-only を除外）
  - root row で許可するか
  - shortcut（例: `t`, `a`, `C`）
  - picker 定義（agent CLI picker、terminal open/new、close safe/force）
  - 実行時に生成する logical effect
- `HomeState::focus_menu_commands` は registry からの `CommandInfo` と `SessionActionSpec` を突き合わせるだけに寄せる。
- renderer が必要とする picker 候補も spec から取れるようにし、`TERMINAL_MENU_ACTIONS` のような分散定数を減らす。

## 対象ファイル

- `src/presentation/tui/home/state/mod.rs`
- `src/presentation/tui/home/state/modal.rs`
- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/command/mod.rs`
- `src/presentation/tui/home/command/builtins.rs`
- `src/presentation/tui/home/ui/panes.rs`
- `src/presentation/tui/home/event/tests/focus_menu.rs`

## 受け入れ条件

- Focus menu の表示行、root row での非表示、`chat` availability gate、`ai` の menu 非表示が現状と一致する。
- `agent` / `terminal` / `close` picker の候補が `SessionActionSpec` 側から導出される。
- handler が command 名文字列の集合を直接 match する箇所が減る。
- #138 の characterization test が通る。

## テスト方針

- `cargo test focus_menu`
- `cargo test presentation::tui::home::state::tests::focus`
- `SessionActionSpec` の単体テストで全 action の表示条件・picker 候補を検証する。

## 非目標

- この issue では action 実行 dispatcher までは置換しない。
- Prompt UI の入力編集や completion は変更しない。
- UI の表示文言・並び順は変えない。
