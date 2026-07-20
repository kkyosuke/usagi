---
number: 403
title: usagi mcp: session 観測・prompt・delegate を接続する（list/status/complete/pr/note/todo/decision/delegate/prompt）
status: in-progress
priority: high
labels: [mcp]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:53:54.200783+00:00
updated_at: 2026-07-20T08:51:28.625814+00:00
---

親: #400。依存: #401。session の観測・追加指示・委譲の MCP tool を daemon/core に接続する。delegate 系は `session_prompt` backend に依存するため本 issue で束ねる。

## 対象 tool（15）

- 観測: `session_list` / `session_status` / `session_complete` / `session_pr`
- note/todo/decision: `session_note_get` / `session_note_update` / `session_todo_list` / `session_todo_add` / `session_todo_update` / `session_todo_remove` / `session_decision_list` / `session_decision_log`
- 追加指示・委譲: `session_prompt` / `session_delegate_issue` / `session_delegate_brief`

## 現状の断絶（根拠）

- これらは `serve.rs:338-346` の `session_action` に載っておらず（`session_prompt` を除く）、`dispatch(name,…)`→`ToolError::Unimplemented`（明示エラー）。
- `session_prompt` は routing 済みだが `SessionRuntime::handle` が `Setup|Prompt => Err(InvalidRequest)`（`crates/daemon/src/usecase/session_runtime.rs:234-236`）。
- `session_list` は `SessionRuntime::handle` が `List/Overview` を実装済み → **routing を足すだけ**の低コスト。

## 完了条件（系統別）

- [ ] `session_list` / `session_status`: daemon の durable session snapshot（agent phase・worktree status/dirty/merged）を返す。まず `session_list` を `SessionAction::List` に routing。
- [ ] `session_note_*` / `session_todo_*` / `session_decision_*`: session worktree の note/todo/decision ストア（core usecase）を読み書きする（session 内でのみ有効という現契約を維持）。
- [ ] `session_pr` / `session_complete`: PR inventory / 完了報告の実データを返す。
- [ ] `session_prompt`: 追加指示を対象 session の agent へ配送する backend を実装（launch queue / live queue、`SessionAction::Prompt` の `InvalidRequest` を実処理へ置換）。
- [ ] `session_delegate_issue` / `session_delegate_brief`: issue→prompt 化 / brief ラップ → session 作成 → prompt 配送を 1 tool で完結。`session_prompt` backend の上に構築。
- [ ] **production E2E**: `usagi mcp` から `session_create`→`session_prompt`（配送先の durable 確認）／`session_delegate_brief`→新 session 生成＋prompt キュー投入、`session_status` の観測を固定。
- [ ] docs（`07-mcp.md`・orchestration guide の observe/delegate）を実挙動へ一致。coverage 100%。
