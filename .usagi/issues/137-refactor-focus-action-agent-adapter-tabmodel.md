---
number: 137
title: refactor: Focus action・Agent adapter・TabModel のリデザイン実装計画
status: in-progress
priority: high
labels: [refactor, review, epic]
dependson: []
related: [119, 120, 122, 128, 129]
created_at: 2026-07-06T00:19:18.467735+00:00
updated_at: 2026-07-08T22:16:30.167232+00:00
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

## 実装計画（子 issue ロードマップ）

この epic は #138–#147 の 10 子 issue に分割済み（すべて `parent: 137`）。3 系統（TUI Focus/tab・agent adapter・orchestration）を横断するが、各 PR は 1 系統 1 責務に閉じ、一度に大規模置換しない。

### 依存グラフ

```
土台（波0：並行可）
  #138 characterization matrix (test)         #139 AgentCapability / LaunchPlan 土台 [DONE]
      |                                              |
      +--------------+--------------+                +--------------+--------------+
      v              v          (builder を使う)      v              v              v
  #140 SessionAction  #141 TabModel                #142 Claude/    #145 Gemini/    #146 session
       定義表・menu         純粋化                       Codex builder    Antigravity      override 検証
       data-driven         (live/+new/pending)          移行              統合             <-> capability
      |              |                                (rel 120,122)   (rel 119,134)   (rel 99,134,135)
      v              v
  #143 SessionAction  #144 TerminalPool
       dispatcher          -> TabModel 適用 +
       (menu/prompt 統合)   PTY IO ownership 分割
      |              |      (rel 128,129)
      +------+-------+
             v
  #147 Home event handler を reducer + effect runner へ分離
```

### 実行ウェーブ（依存が解ける順）

| 波 | 子 issue | 前提 | 並行可否 |
|---|---|---|---|
| 0（土台） | #138 characterization test / #139 capability・LaunchPlan 土台（#139 は done） | なし | 2 本並行可 |
| 1 | #140（←#138）/ #141（←#138）/ #142（←#139）/ #145（←#139）/ #146（←#139） | 波0 完了 | TUI と agent 系で write-set が別。#140/#141 は TUI、#142/#145/#146 は agent/orchestration で相互に並行可 |
| 2 | #143（←#140）/ #144（←#141） | 波1 の対応 issue | 並行可（片方 Focus dispatcher、片方 TerminalPool） |
| 3 | #147（←#143, #141） | #143 と #141 | 単独 |

### PR サイズ・系統・リスク

| 子 issue | 系統 | 目安サイズ | 種別 | リスク緩和 |
|---|---|---|---|---|
| #138 | TUI test | M | 追加のみ | 挙動不変、後続の安全網 |
| #139 | agent | S–M | 型追加 | 旧 adapter は既存実装のまま |
| #140 | TUI | M | 抽出 | #138 で表示条件を固定済み |
| #141 | TUI/terminal | M | 抽出 | 純粋 model のみ、PTY 非移動 |
| #142 | agent | M–L | 段階移行 | command string を snapshot で固定 |
| #143 | TUI | M | 統合 | dispatcher は effect を返すだけ（IO 非実行） |
| #144 | TUI/terminal | M | 委譲 | 公開 API 維持、index 演算を TabModel へ |
| #145 | agent | S–M | 統合 | #119 と同粒度、MCP 注入は非対象 |
| #146 | orchestration/mcp | M | 接続 | parse は presentation、可用性判断は usecase/domain |
| #147 | TUI | L | 段階移行 | Focus 周辺から着手、全 event loop rewrite はしない |

### 既存 refactor issue との関係

| 既存 issue | 内容 | この epic での扱い |
|---|---|---|
| #119 | Gemini/Antigravity アダプタのパラメータ化統合 | #145 が builder 土台（#139）に載せて実移行。#145 は #119 の実装作業として取り込める粒度 |
| #120 | model_flag_parts 三重定義・MCP 台帳・phase 語彙の SSoT 化 | #142 が Claude/Codex 共通 vocabulary（MCP server spec / phase hook / model flag）へ移す形で吸収 |
| #122 | agent/codex.rs・claude.rs を launch/session に分割 | #142 の builder 移行が分割の土台。組織的分割は #122、builder 化は #142 と役割分担 |
| #128 | terminal/pool.rs から watcher・PR scan 分離 | #144 は tab 状態の責務分離に限定し、#128 の watcher/PR scan 分離とは write-set が別（競合回避） |
| #129 | pane.rs::pump_input の状態構造体化・ハンドラ分割 | #141/#144 と対象ファイルが別（tab 状態 vs 入力ポンプ）。並行可能 |

（各子 issue の `related` にも上記対応を明記済み。）

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

- 子 issue が実装しやすい PR サイズに分割され、依存関係が明示されている。→ 上記「実装計画」の依存グラフ・ウェーブ・PR サイズ表で満たす。
- Focus action、agent builder、tab model、event reducer のどれも一度に大規模置換しない計画になっている。→ 4 系統をそれぞれ複数 PR に分割し、各 PR は characterization test → 抽出 → 置換の順に段階化。
- 既存の #119/#120/#122/#128/#129 など、既にある refactor issue との重複・関連が子 issue の本文に記載されている。→ 上記「既存 refactor issue との関係」表および各子 issue 本文・`related` に記載済み。
- coverage 100% を維持するため、各子 issue にテスト方針が書かれている。→ #138–#147 すべてに「テスト方針」節あり（#138 が現状仕様を固定し、以降で純粋 reducer/builder/model 単体テストへ移行）。

## テスト方針

親 issue 自体はテストを持たない。子 issue では、最初に現状仕様を固定する characterization test（#138）を追加し、その後の PR で純粋 reducer / builder / model の単体テストへ移す。coverage 100% は各子 issue 側で担保する。

## 非目標

- この親 issue だけで実装を変更しない。
- UI の見た目やキー割り当てを変更しない。
- agent CLI の仕様変更や新 CLI 追加は行わない。
- `TerminalPool` の watcher / PR scan / notification の挙動変更は行わない。
