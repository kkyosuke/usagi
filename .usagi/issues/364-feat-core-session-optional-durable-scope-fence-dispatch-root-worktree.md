---
number: 364
title: feat(core): session-optional な durable scope/fence/dispatch 語彙と root worktree の永続化
status: in-progress
priority: high
labels: [core, workspace-root, agent, terminal]
dependson: []
related: []
parent: 363
created_at: 2026-07-19T21:04:49.858195+00:00
updated_at: 2026-07-19T21:05:49.182469+00:00
---

## 目的

workspace root（session を持たない scope）を安全に fence できるよう、共有の identity/fence/scope/dispatch 語彙に `session_id: Option<SessionId>` を通し、`None` を workspace root とする。root チェックアウトの `WorktreeId` を lifecycle state に永続化する。session scope の意味論は回帰させない（session 経路は常に `Some`）。

## 変更内容

- `crates/core/src/domain/id/mod.rs`
  - `CompletionFence.session_id: SessionId → Option<SessionId>`。
  - `AgentRuntimeRef.session_id: SessionId → Option<SessionId>`、`AgentRuntimeRef::new` の所有検証を `terminal.session_id == session_id`（Option 同士）へ。
- `crates/core/src/domain/agent/mod.rs`
  - `Agent.session_id` / `CallerRef.session_id` / `WorkerRef.session_id` / `LaunchScope.session_id` を `Option<SessionId>` 化。
- `crates/core/src/usecase/client.rs`
  - `AgentLaunchIntent.session: SessionId → Option<SessionId>`。
- `crates/core/src/infrastructure/store/dispatch.rs`
  - `inbox_path` を Option-aware に（`None` は UUID と衝突しない予約キー。例: `"workspace-root"`）。
  - `upsert_agent_by_runtime_model` を `Option<SessionId>` 受けに。
- `crates/core/src/domain/session_lifecycle.rs`
  - `WorkspaceLifecycleState` に `root_worktree_id: WorktreeId` を追加（後方互換のため deserialize は `Option`、daemon open 時に一度だけ生成・永続化）。lifecycle reducer の fence 比較を Option 対応（session 経路は `Some` 必須・等価）。

## 完了条件

- session 経路のシリアライズ／fence／dispatch は既存テストで回帰しない。
- `session_id: None` の fence / scope / caller / worker / agent が構築・比較でき、inbox が予約キーに分離される。
- 既存 `dispatch.json` / `sessions.json`（root worktree なし）を読み込め、root worktree を一度だけ backfill する。
- coverage 100%。

## 依存

Epic #363。後続の daemon/tui はこの語彙に依存する。
