---
number: 137
title: refactor: Focus action・Agent adapter・TabModel のリデザイン実装計画
status: todo
priority: high
labels: [refactor, review, epic]
dependson: []
related: []
created_at: 2026-07-06T00:19:18.467735+00:00
updated_at: 2026-07-06T00:19:18.467735+00:00
---

## 目的

Focus の action/prompt、各 agent adapter、tab / terminal pane 操作を「巨大 handler と adapter 個別実装」から、data-driven な action 定義・純粋な状態遷移・薄い IO runner に分割するための親 issue。

## 背景

レビュー対象は合計約 19k 行あり、特に `event/mod.rs`（約 1.7k 行）、`event/handlers.rs`（約 1.7k 行）、`state/mod.rs`（約 3.9k 行）、`terminal/pool.rs`（約 1.5k 行）、`terminal/pane.rs`（約 1.2k 行）、agent adapter 群（合計約 2.9k 行）に状態遷移・IO・表示都合が混在している。

主な複雑さは以下。

- Focus menu と Focus prompt が同じ Session command / `Effect` を別々の match で実行しており、`terminal` / `agent` / `ai` / `chat` / `diff` / `close` の動作差分が handler 側に散る。
- `FocusMenu` は cursor だけを持つが、展開可否・候補・実行内容は `HomeState` と `handlers.rs` の複数関数に分散し、action の追加時に renderer / state / handler / command の複数箇所を同期する必要がある。
- tab 操作は `terminal/tabs.rs` に純粋関数がある一方、Focus の `+ new` 仮想タブ、pool の PTY 所有、tab menu の rename/close/move、Attached の drag/drop が別々に状態を持つ。
- agent adapter は command line build、MCP / hook / system prompt 注入、model flag、resume 探索、forget、headless が各 adapter に混在し、capability matrix と実際の builder が分離している。
- event loop は key を直接 effect 実行まで持っていくため、coverage 100% を維持するには端末・PTY・background task を大量 fake する必要がある。

## 変更方針

この親 issue は実装を直接行わず、子 issue の実装単位を束ねる。

設計方針:

1. Focus action は `SessionActionSpec` の定義表に寄せる。
   - command 名、menu 表示可否、root row 可否、shortcut、sub-picker、実行 effect を 1 か所に集約する。
   - menu と prompt は同じ dispatcher に `SessionActionRequest` を渡し、`SessionActionEffect` を受ける。
2. UI event handler は small reducer + effect runner に分ける。
   - reducer は `HomeState` と input から `Effect` enum を返す純粋/準純粋ロジックにする。
   - PTY 起動、git diff、config 画面、background task、browser open は runner に隔離する。
3. agent は `AgentCapability` + `LaunchPlan` builder に分ける。
   - CLI 固有の構文差分は descriptor に寄せる。
   - MCP/hook/system prompt/model/headless/resume/forget は trait composition または小 port に分割する。
4. tab は `TabModel` / `PaneRegistry` 的な純粋状態に切り出す。
   - live pane list、active index、label override、仮想 `+ new`、pending tab を IO なしでテストする。
   - `TerminalPool` は PTY 所有・spawn・watcher 登録に集中する。
5. 各 PR は先に characterization test を足し、その後に抽出・置換する。

## 対象ファイル

- `src/presentation/tui/home/event/mod.rs`
- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/event/tests/focus_menu.rs`
- `src/presentation/tui/home/event/tests/focus_prompt.rs`
- `src/presentation/tui/home/pane_input.rs`
- `src/presentation/tui/home/command/mod.rs`
- `src/presentation/tui/home/command/builtins.rs`
- `src/presentation/tui/home/state/modal.rs`
- `src/presentation/tui/home/state/mode.rs`
- `src/domain/agent.rs`
- `src/domain/agent_feature.rs`
- `src/infrastructure/agent/*.rs`
- `src/usecase/agent.rs`
- `src/usecase/settings.rs`
- `src/presentation/mcp/session.rs`
- `src/presentation/mcp/usagi.rs`
- `src/presentation/tui/home/terminal/*.rs`

## 受け入れ条件

- 子 issue が実装しやすい PR サイズに分割され、依存関係が明示されている。
- Focus action、agent builder、tab model、event reducer のどれも一度に大規模置換しない計画になっている。
- 既存の #119/#120/#122/#128/#129 など、既にある refactor issue との重複・関連が子 issue の本文に記載されている。
- coverage 100% を維持するため、各子 issue にテスト方針が書かれている。

## テスト方針

親 issue 自体はテストを持たない。子 issue では、最初に現状仕様を固定する characterization test を追加し、その後の PR で純粋 reducer / builder / model の単体テストへ移す。

## 非目標

- この親 issue だけで実装を変更しない。
- UI の見た目やキー割り当てを変更しない。
- agent CLI の仕様変更や新 CLI 追加は行わない。
- `TerminalPool` の watcher / PR scan / notification の挙動変更は行わない。
