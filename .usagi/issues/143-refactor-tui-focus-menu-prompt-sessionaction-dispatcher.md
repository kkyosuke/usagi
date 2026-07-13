---
number: 143
title: refactor(tui): Focus menu/prompt の実行経路を SessionAction dispatcher に統合する
status: todo
priority: high
labels: [refactor, tui, review]
dependson: [140]
related: []
parent: 137
created_at: 2026-07-06T00:21:25.629709+00:00
updated_at: 2026-07-06T00:21:25.629709+00:00
---

## 目的

Focus menu と Focus prompt が同じ session action を別々の match で実行している状態を解消し、`SessionActionDispatcher` で logical effect を一元化する。

## 背景

`focus_prompt_key` は `state.focus_prompt_submit()` の `Effect` を再 match し、`run_focus_command` は command 名文字列を再 match している。`terminal` / `agent` / `diff` / `close` の実行が menu と prompt で似た形に重複し、`ai <prompt>` だけ prompt-only という差分も handler 側の知識になっている。

## 変更方針

- #140 の `SessionActionSpec` を使い、`SessionActionRequest` を導入する。
  - source: Menu / Prompt / Shortcut / Prefix
  - command/effect: `CommandResult::effect` または spec の action id
  - selected picker option / prompt text / selected agent CLI
- dispatcher は IO を直接実行せず、`SessionActionEffect` を返す。
  - `LaunchPane { agent, cli, initial_prompt }`
  - `OpenExternalTerminal`
  - `OpenDiff`
  - `CloseSession { force }`
  - `LogComingSoon` など
- `focus_menu_key` と `focus_prompt_key` は input 編集・menu cursor 更新だけを行い、実行は dispatcher + effect runner に寄せる。
- `run_focus_command` の文字列 match を段階的に削除する。

## 対象ファイル

- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/state/mod.rs`
- `src/presentation/tui/home/command/mod.rs`
- `src/presentation/tui/home/command/builtins.rs`
- `src/presentation/tui/home/event/tests/focus_menu.rs`
- `src/presentation/tui/home/event/tests/focus_prompt.rs`

## 受け入れ条件

- menu から実行しても prompt から実行しても同じ `SessionActionEffect` 経路を通る。
- `terminal new` / `agent <cli>` / `ai <prompt>` / `close --force` の差分が dispatcher の単体テストで見える。
- 既存挙動（ログ、palette persist、prompt clear、menu filter clear、root row close 拒否）が維持される。
- #138 と #140 のテストが通る。

## テスト方針

- dispatcher の純粋単体テストを追加する。
- `cargo test focus_menu`
- `cargo test focus_prompt`
- prompt submit が history/persist を維持する既存テストを確認する。

## 非目標

- event loop 全体の reducer 化は行わない。
- `CommandRegistry` の構造や command syntax を変更しない。
- 実際の PTY spawn / diff 実行の実装は変更しない。
