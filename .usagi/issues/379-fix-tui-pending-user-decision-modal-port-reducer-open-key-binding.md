---
number: 379
title: fix(tui): pending user decision を本番で配送・自動 modal 表示する（port 注入・reducer 自動 open・key binding）
status: done
priority: high
labels: [tui, daemon, ux, bug]
dependson: [378]
related: [329, 330, 328]
created_at: 2026-07-19T22:45:03.430438+00:00
updated_at: 2026-07-20T00:50:10.891462+00:00
---

## 背景 / 不具合

`user_decision_get` が成功を返すのに回答 modal が出ない不具合の TUI 層。triage の全体像は
[document/proposals/09-user-decision-mcp.md](../../document/proposals/09-user-decision-mcp.md#実装状況と未接続点triage)。
daemon 側の配送は #378。本 issue は **TUI 本番接続と自動表示**を担当する。

### 確認された根本原因（この層）

- `run_workspace_controller`（`crates/tui/src/presentation/mod.rs`）は decision port を引数に取らない。
  `DaemonBackend`（`Effect::RefreshDecisions`/`ResolveDecision` を `DecisionPort` へ流す executor）は本番で
  構築されず（`DaemonBackend::new` は test のみ）、本番の `DecisionPort` は常に `NoDecisions`（no-op）。
  → decision の取得・回答が本番で捨てられる。
- `DaemonPush::DecisionsSnapshot`/`DecisionResolved`/`DecisionError` と reducer adapter は TUI に存在するが、
  wire から `DaemonPush::Decisions*` を構築する transport decoder が無い。
- `BackendEvent::Decisions` の reducer 分岐（`crates/tui/src/usecase/application/controller.rs`）は
  `state.decisions`（一覧）だけを更新し、`reconcile_decision_overlay` は**既に開いている** overlay しか調整しない。
  pending が増えても `Overlay::Decisions` を開かない。手動の `AppKey::OpenDecisions` も key binding が無く到達不能
  （`app_event_from_key`/`live_action_to_app_key` に未登録、`Char` alias も無い）。

## やること

- `run_workspace_controller` に `DecisionPort`（または decision command port）を追加し、合成ルート
  （`src/runtime/tui.rs`）で daemon-backed 実装を注入する。`Effect::RefreshDecisions`/`ResolveDecision` を
  本番で daemon の user_decision list/resolve（#378）へ届ける。
- TUI transport decoder に daemon の decision push（#378 が送る `DecisionsSnapshot`/`DecisionResolved`/`DecisionError`）
  を `DaemonPush::Decisions*` へ decode する経路を足す。reconnect/resync で pending 一覧を snapshot 置換で復元する。
- reducer: 対象 workspace に pending decision が到着したら `Overlay::Decisions` と `DecisionOverlayState` を
  **自動 open** する分岐を追加する。既存の modal input ownership を尊重し、他の overlay/editor が前面のときは
  奪わず、閉じられたら再表示できる（dismiss は UI のみを閉じ daemon state を変えない）。
- 手動 open の key binding を追加する（`OpenDecisions` に `Char` alias と `app_event_from_key` 登録）。

## 受け入れ条件

- 対象 workspace に pending decision が届くと回答 modal が自動で前面に出る。dismiss 後も一覧から再表示できる。
- 未許可 freeform・空回答・不正 option を送らない。resolve confirmation event でのみ一覧から除く。
  cancel/expire/resolved event は表示を正しく収束する。
- restart/reconnect/resync、duplicate/stale response、自動 open と手動 open の競合を deterministic test で固定し
  coverage 100% を維持する。
- reducer / render / fake-daemon (`DecisionPort` fake) / runtime integration の regression test を追加する。

## テスト方針

- `cargo test -p usagi-tui`
- push/PR 前は full gate（coverage 100%）と Markdown link check。

## 非目標

- daemon durable state・MCP dispatch_tool 接続（#378）。
- supervisor を開始・resume する UI。
