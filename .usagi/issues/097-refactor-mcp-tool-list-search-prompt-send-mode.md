---
number: 97
title: refactor(mcp): tool 面をワークフロー志向に統合（list→search 吸収・prompt/send を mode 統合）
status: done
priority: medium
labels: [refactor, mcp, review]
dependson: []
related: []
created_at: 2026-07-04T00:53:29.238209+00:00
updated_at: 2026-07-04T01:49:36.993358+00:00
---

MCP tool の粒度レビューを受けた統合。CLI/MCP は別 IF として扱い、CLI は現状維持・MCP のみ最適化。内部ロジックは既存 usecase（SSoT）に委譲したまま。tool 数 21 → 18（issue 6 + memory 4 + session 7 + orchestration 1）。

## 変更内容（実装済み）

### 重複の統合
- **B: `issue_list` を廃止**し `issue_search` の `query` を任意化（省略で全件）。空クエリ全件一致の `usecase::search::matches_folded` に集約済み。
- **C: `memory_list` を廃止**し `memory_search` の `query` を任意化（同上）。
- **E: `memory_update` を廃止**し `memory_save` を upsert 1 本に。既存は**指定フィールドだけ部分更新**（未指定は保持）、新規は `title` 必須。MCP ハンドラが usecase の `update`/`save` を使い分けて実現（SSoT 維持）。

### 選択肢の統合
- **A: `session_send` を廃止**し `session_prompt` に `mode`（`auto`（既定）/`queue`/`live`）を統合。`auto` は `agent_state_store`（worktree 別 agent phase、ペイン終了でクリア）で live pane を検知して配送先を自動選択。返り値 `{name, delivered_to, detail}` で実チャネルを可視化。`AgentBackend` に `agent_is_live()` を追加、mode→チャネル判定はテスト可能な session サーバ側。

### 手順の統合
- **D: `session_delegate_issue` を追加**（合成サーバ usagi.rs）。`issue_to_prompt` → `session_create` → `session_prompt(mode=queue)` を 1 呼び出しに。新規ロジックなし（既存 tool を順に呼ぶだけ）。primitive は存置し、細かい制御時はそれらを使う。

## 確認
- `cargo fmt` / `clippy -D warnings` / `test`（2704 件）green。pre-push で coverage 100%。
- テストも新 IF に合わせて更新。

## 補足
CLI の `issue list` / `memory list` / `memory update` は人間向けに存置（IF ごとに最適化）。issue/memory の素の CRUD は「エージェントが所有するデータストアの操作」として妥当なため残置。
