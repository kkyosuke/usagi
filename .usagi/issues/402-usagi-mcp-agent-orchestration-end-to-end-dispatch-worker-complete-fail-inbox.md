---
number: 402
title: usagi mcp: agent orchestration を end-to-end 接続する（dispatch→worker→complete/fail→inbox）
status: done
priority: high
labels: [mcp]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:53:40.373314+00:00
updated_at: 2026-07-20T09:18:01.804504+00:00
---

親: #400。依存: #401（安全弁）。agent orchestration の MCP tool を実 `AgentRuntime`/`DispatchStore` に接続する。

## 対象 tool（7）

`session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox`。

## 現状の断絶（根拠）

- 実 dispatch 経路 `dispatch_dispatch`（`src/runtime/daemon.rs:1319`）は kind `dispatch`（`DaemonRequest::Dispatch`）で駆動し、session 解決→`AgentRuntime::dispatch`→worker PTY 起動まで実装済み。しかし **`DaemonRequest::Dispatch` を構築する client がコードベースに存在しない**（`daemon.rs:1331` は受信側の分解のみ、送信側ゼロ）。
- MCP `session_dispatch` は `DispatchTool::Dispatch`（kind `dispatch_tool`）を送り（`serve.rs:351`）、`dispatch_user_decision` の fallthrough でエコー no-op になる（#401 で一旦エラー化）。
- `agent_list/get/complete/fail/inbox` も同様に `DispatchTool` の非 decision action として no-op。

## 完了条件（系統別）

- [ ] `session_dispatch`: MCP 呼び出しが実 worker 起動まで到達する。`DispatchTool::Dispatch` を実 dispatch 経路（`AgentRuntime::dispatch` / `DispatchStore`）へ接続する（`dispatch_tool` handler で実処理するか、`session_dispatch` を実 `DaemonRequest::Dispatch` 経路へ載せ替える）。daemon が resolve した session worktree で worker が spawn され、`run_id`/`terminal` を含む実応答を返す（`daemon.rs:1373-1384` の admission ペイロード相当）。caller は `caller_context` credential から解決（クライアント供給の caller 名を信用しない）。
- [ ] `agent_list` / `agent_get`: `DispatchStore` の実 run/agent 状態を返す。
- [ ] `agent_complete` / `agent_fail`: worker の完了/失敗を durable に記録し、**caller inbox（`AgentInbox`, `crates/core/src/infrastructure/store/dispatch.rs`）へ配送**する。
- [ ] `agent_inbox`: caller が自 inbox の配送済みレポートを取得できる。
- [ ] **production E2E**: `usagi mcp` 実プロセスから `session_dispatch`→worker 起動→（fixture worker で）`agent_complete`→caller の `agent_inbox` にレポート到達、までを stdio→実 daemn→durable で固定。
- [ ] docs（`07-mcp.md`・orchestration guide）の dispatch/observe/complete 手順を実挙動に一致させる。coverage 100%。

## 留意

- 稼働中 daemon が main より先行しており、agent 系が別ブランチで進行している可能性。着手前に in-flight を突き合わせること。
