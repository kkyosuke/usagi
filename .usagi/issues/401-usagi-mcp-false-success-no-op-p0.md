---
number: 401
title: usagi mcp: false-success no-op を明示エラー化する（安全弁・P0）
status: in-progress
priority: high
labels: [mcp]
dependson: []
related: []
parent: 400
created_at: 2026-07-20T04:53:15.305558+00:00
updated_at: 2026-07-20T07:09:00.119250+00:00
---

親: #400。**最優先の安全弁**。実 durable 効果が無いのに成功を返す 13 tool を、実装が入るまで**明示エラー**に変える（agent の誤成功を止める）。以後の系別実装 PR は、自系 tool についてこのエラーを実処理へ置換する。

## 対象（false-success no-op = 13）

- DispatchTool 非 decision アクション（7）: `session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox`
- supervisor（6）: `supervisor_start` / `supervisor_get` / `supervisor_list` / `supervisor_cancel` / `supervisor_resolve_escalation` / `supervisor_events`

## 根本原因（該当箇所）

- `dispatch_user_decision`（`src/runtime/daemon.rs:1174-1184`）が `UserDecision*` 以外を `usagi_daemon::presentation::ipc::dispatch()` に丸投げ。
- 既定 `ipc::dispatch()`（`crates/daemon/src/presentation/ipc.rs:89-109`）は kind が `{session,agent,dispatch}` 以外だと `ResponseOutcome::Ok` で **body をエコー**。
- `SupervisorTool`（kind `supervisor_tool`）は router（`daemon.rs:1046-1056`）に arm が無く `_ => ipc::dispatch()` へ落ちる。

## 方針

no-op の間は、これらの action に対して daemon が `ResponseOutcome::Error`（例 `ErrorCode::Unimplemented` 相当。無ければ `Unavailable`/`InvalidArgument` で "not implemented" を明示）を返し、MCP serve.rs（`tools/call`）がそれを JSON-RPC エラーとして返す。**リクエストをエコーした Ok・偽 Accepted を返さない**こと。実装は次のいずれかで達成できる（実装 issue の設計に委ねる）:

- `dispatch_user_decision` の非 decision fallthrough を「エコー」ではなく明示エラーへ変更。
- router に `supervisor_tool` arm を足し、未 compose の間は明示エラーを返す handler を置く。
- もしくは既定 `ipc::dispatch()` が `dispatch_tool`/`supervisor_tool` kind を**エコーせずエラー**にする（他 kind の Accepted 契約は壊さない）。

## 完了条件

- [ ] 上記 13 tool を新規ビルド `usagi mcp` stdio の `tools/call` に投げると **明示的な JSON-RPC エラー**（成功・エコーでない）を返す。
- [ ] `session_create`/`session_remove`/`session_recover_legacy`（実装済み）と、`session`/`agent`/`dispatch` kind の Accepted 契約が回帰しない（既存 daemon test green）。
- [ ] 変更を固定する daemon/serve のユニットテスト（no-op action → Error）を追加。
- [ ] 影響する docs（あれば）を同 PR で更新。coverage 100% 維持。
