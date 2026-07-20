---
number: 405
title: usagi mcp: supervisor runtime を production に配線し supervisor_* を接続する
status: todo
priority: medium
labels: [mcp]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:54:26.630615+00:00
updated_at: 2026-07-20T04:54:26.630615+00:00
---

親: #400。依存: #401。`SupervisorRuntime` を daemon production composition に配線し、`supervisor_*` tool を実処理へ接続する。

## 対象 tool（6）

`supervisor_start` / `supervisor_get` / `supervisor_list` / `supervisor_cancel` / `supervisor_resolve_escalation` / `supervisor_events`。

## 現状の断絶（根拠）

- MCP は `DaemonRequest::SupervisorTool`（kind `supervisor_tool`）を送る（`serve.rs:284-303`）が、router（`src/runtime/daemon.rs:1046-1056`）に `supervisor_tool` arm が無く `_ => ipc::dispatch()` のエコー no-op に落ちる（#401 で一旦エラー化）。
- `SupervisorRuntime`（`crates/daemon/src/usecase/supervisor_runtime.rs`）は実装・test 済みだが、production composition（`spawn_ipc_server` / `start_ipc_accept_loop`）で**一度も生成・保持されていない**（`SupervisorRuntime::new` は test のみ）。

## 完了条件

- [ ] `spawn_ipc_server`（`src/runtime/daemon.rs:783`）で `SupervisorRuntime` を生成・共有し、必要な `tick` 駆動を daemon に組み込む。
- [ ] router に `supervisor_tool` arm を追加し、`SupervisorToolAction`（Start/Get/List/Cancel/ResolveEscalation/Events）を実 runtime へ接続。caller provenance は IPC context から daemon が導出（クライアント供給フィールドにしない、`client.rs:77-84` の設計に従う）。
- [ ] 各 action が durable な supervisor aggregate 状態を反映した結果を返す（エコー・偽 Ok を返さない）。
- [ ] **production E2E**: `usagi mcp` から `supervisor_start`→`supervisor_get`/`supervisor_list`/`supervisor_events` で状態が観測できることを stdio→実 daemon→durable で固定。
- [ ] docs（`07-mcp.md` に supervisor 系の記述が必要なら追加）。coverage 100%。
- [ ] `Escalated` の解決意味論を確定する（下記「補足」参照）: reducer に ResolveEscalation で Escalated から遷移するイベントを追加するか、「解決＝新 run 起票」を明文化して `resume` choice を落とす。

## 補足（2026-07-20 全体コードレビューで検証した追加根拠）

domain 側にも「解決経路の欠如」がある（2f4dc5b6 時点で検証）。ResolveEscalation を配線する際は daemon の受け口だけでなく domain の遷移も必要になる:

- `crates/core/src/domain/supervisor.rs:94` — `terminal()` が `Self::Escalated` を **terminal 状態**として列挙する（Escalated からの遷移が定義されていない）。
- 一方 `escalate(...)`（fn :790）は `choices: vec!["resume".into(), "cancel".into()]`（:563-570）を積んでおり、**「resume できる」と提示しながら resume の遷移が存在しない**。
- `SupervisorToolAction::ResolveEscalation` は `crates/core/src/usecase/client.rs:178` に定義済みだが、daemon 側の受け口はゼロ（`src/runtime/daemon.rs` は Pr/DispatchTool/Dispatch/Session/Agent の arm のみ :1084/:1163/:1331/:1446/:1535、`SupervisorTool` への言及なし — grep 0 件）。
