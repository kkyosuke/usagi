---
number: 378
title: fix(daemon): user_decision の MCP dispatch_tool を durable store と workspace-scoped 配送に接続する
status: todo
priority: high
labels: [daemon, mcp, cli, supervisor, bug]
dependson: []
related: [329, 330, 328, 326, 327]
created_at: 2026-07-19T22:44:35.043760+00:00
updated_at: 2026-07-19T22:44:35.043760+00:00
---

## 背景 / 不具合

`user_decision_get` が「取得リクエストは正常に dispatch、回答待ち」「テスト用の判断リクエストも作成済み」と
返すのに TUI に回答 modal が出ない。triage の結果、原因はこの機能が**本番の 3 層すべてで未接続**であること
（設計と全体像は [document/proposals/09-user-decision-mcp.md](../../document/proposals/09-user-decision-mcp.md#実装状況と未接続点triage)）。
本 issue はそのうち **MCP → daemon → 配送** の層を担当する（TUI 側は #382）。

### 確認された根本原因（この層）

- 合成ルートの request 分岐（`src/runtime/daemon.rs` の `handle_connection_with_terminal_and` に渡す closure）は
  `kind` が `session`/`agent`/`dispatch`/`metrics`/`pr` のときだけ handler へ回し、`dispatch_tool`
  （`DaemonRequest::DispatchTool` の serde tag）に**分岐が無い**。そのため daemon の echo stub
  `usagi_daemon::presentation::ipc::dispatch` に落ちる。この stub は `kind` を `session|agent|dispatch` でしか
  見ないため `ResponseOutcome::Ok` で **payload をそのまま echo** する。MCP serve は `Ok(body)` を success text に
  包むため、**何も永続化していないのに false success** が返る。
- `UserDecisionStore`（`crates/core/src/infrastructure/store/user_decision.rs`）は本体定義以外から**一度も呼ばれない**。
- `dispatch_tool_action`（`crates/cli/src/mcp/serve.rs`）は `user_decision_request`/`user_decision_get` だけを map し、
  `list`/`resolve`/`cancel`/`expire` の tool 名が未 map。

## やること

- `dispatch_tool_action` に `user_decision_list`/`user_decision_resolve`/`user_decision_cancel`/`user_decision_expire`
  を追加し、対応する MCP tool 定義（`crates/cli/src/mcp/tools/`）を揃える。
- 合成ルート dispatch closure に `Some("dispatch_tool") => dispatch_user_decision(...)` 分岐を追加し、
  `DispatchToolAction::UserDecision*` を `UserDecisionStore` の request/get/list/resolve/cancel/expire へ接続する。
  owner（workspace/session/agent/run/caller）は #329 の実行コンテキストと provenance から解決し、
  agent supplied の owner は受け付けない。`request` は入力待ちせず即時に `{ decision_id, status: "waiting_for_user" }`
  を返す。
- resolve は Pending の compare-and-set で option ID または許可された freeform のみ受け、durable decision 更新と
  `UserDecisionResolved` inbox event append を一度だけ commit する。cancel/expire/親 run 終了は terminal 化し
  resume event を出さない。同じ idempotency key の retry は同一 ID を返す。
- daemon が対象 workspace の pending decision を TUI へ投影できるようにする（`DaemonPush::DecisionsSnapshot` を送る
  push 経路、または reconnect/resync で使う snapshot 応答）。他 workspace の decision を projection に入れない。

## 受け入れ条件

- `user_decision_request` が `UserDecisionStore` に Pending を永続化し、echo stub に落ちない。restart 後に
  pending/terminal state と回答を復元できる。
- 不正 option、freeform 非許可、二重回答、foreign workspace/decision、cancel/expire 後の回答は typed safe error で
  state と inbox を変更しない。resolve では durable decision と inbox event が片方だけにならず二重配送しない。
- 対象 workspace の pending が snapshot/push として TUI transport へ届く形になっている。
- MCP contract、durable store、IPC、resolve compare-and-set、cancel/expire/parent exit、duplicate/restart を
  deterministic test で固定し coverage 100% を維持する。

## テスト方針

- `cargo test -p usagi-core`
- `cargo test -p usagi-daemon`
- `cargo test -p usagi-cli mcp`
- push/PR 前は full gate（coverage 100%）と Markdown link check。

## 非目標

- TUI の port 注入・reducer 自動 open・key binding（#382）。
- supervisor による agent process の自動 resume。
