---
number: 321
title: feat(core): agent dispatch の durable ドメイン型と store を追加する
status: in-progress
priority: high
labels: [core, mcp, orchestration, agent]
dependson: []
related: [109, 110, 146, 271]
parent: 105
created_at: 2026-07-18T00:00:00+00:00
updated_at: 2026-07-17T21:14:51.036057+00:00
---

## 目的

他 session の特定 agent への即時 dispatch と、呼び出し元への確実な完了報告を支える **durable な
ドメイン型と store** を `usagi-core` に追加する。設計の正本は
[document/proposals/08-agent-dispatch-mcp.md](../../document/proposals/08-agent-dispatch-mcp.md)（§4）。

本 issue は基盤のみを実装し、daemon runtime（#322）と MCP tool（#323）への配線は行わない。

## 背景

現状 `usagi-core` に「agent 単位の永続エンティティ」「run を返す dispatch」「inbox」は無い
（inbox は grep 0 件）。一方で `OperationId`（UUIDv7）・`AgentProfileId`・`ModelSelector`・
`CompletionFence`・`SessionId`・`json_file`／`store_lock` は既にあり、これらの上に組む。

## やること

- ドメイン型（`crates/core/src/domain/` 配下、`chrono`/`serde`/`uuid` のみ依存）:
  - `AgentId`（UUIDv4 incarnation。既存 id マクロに追加）。
  - `Agent { agent_id, session_id, runtime: AgentProfileId, model: ModelSelector, status: AgentStatus, current_run: Option<OperationId> }`。
  - `AgentStatus`（`Idle` / `Running` / `Exited` / `Failed`）。
  - `DispatchRun { run_id: OperationId, agent_id, prompt, started_at, ended_at?, status: RunStatus }`。
  - `RunStatus`（`Running` / `Completed` / `Failed` / `NoReport`）。
  - `DispatchBinding { run_id: OperationId, caller: CallerRef, worker: WorkerRef }`、`CallerRef`/`WorkerRef { session_id, agent_id }`。
  - `InboxMessage { run_id, from: WorkerRef, kind: InboxKind, summary, result: Option<StructuredResult>, created_at, read }`、`InboxKind`（`Completed`/`Failed`/`NoReport`）。
  - `StructuredResult { pr?, commits: Vec<String>, changed_files: Vec<String>, verification? }`。
- durable store（`crates/core/src/infrastructure/store/` 配下、daemon state dir に永続）:
  - agent / dispatch run / binding のレジストリ（`json_file` atomic write + `store_lock`）。
  - caller の (session, agent) 単位の inbox（例 `<daemon-state>/inbox/<caller_session_id>/<caller_agent_id>.jsonl`、atomic append + lock）。
  - upsert（agent by id / by runtime+model）・run 追加・状態遷移・inbox append / read マーク・未読取得の操作を提供する。
- ユニットテストでカバレッジ 100%（round-trip serialize、upsert、状態遷移、inbox append/read、
  未読フィルタ、cross-process lock 経路を fake/tempfile で網羅）。

## 受け入れ条件

- 新ドメイン型が `serde` で round-trip し、`domain/` の依存ルール（重い外部クレートを持ち込まない）を守る。
- store が atomic write / lock を使い、既存 `sessions.json` と同じ永続基盤に乗る。
- inbox は caller の (session, agent) 単位で durable に残り、書き手プロセスの生死に依存せず読み出せる。
- カバレッジ 100%（lines/functions）。`#[coverage(off)]` は実 IO 薄ラッパに限定し理由を記す。

## 非目標

- MCP tool・daemon runtime への配線（#322 / #323）。
- 既存 `session_*` / `session_delegate_*` の挙動変更。

## テスト方針

- `cargo test -p usagi-core domain::agent`
- `cargo test -p usagi-core infrastructure::store`
- push/PR 前は [品質チェック](../../document/06-conventions.md#品質チェックリスク比例の-gate)の full gate。
