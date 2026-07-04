---
number: 97
title: refactor(mcp): tool 面をワークフロー志向に統合（list→search 吸収・prompt/send を mode 統合）
status: done
priority: medium
labels: [refactor, mcp, review]
dependson: []
related: []
created_at: 2026-07-04T00:53:29.238209+00:00
updated_at: 2026-07-04T00:53:37.545940+00:00
---

MCP tool の粒度レビューを受けた統合。CLI/MCP は別 IF として扱い、CLI は現状維持・MCP のみ最適化。内部ロジックは既存 usecase（SSoT）に委譲したまま。

## 変更内容（実装済み）

- **B: `issue_list` を廃止**し `issue_search` の `query` を任意化（省略で全件）。空クエリが全件一致する `usecase::search::matches_folded` に集約済みのため、search 1 本で list を包摂。
- **C: `memory_list` を廃止**し `memory_search` の `query` を任意化（同上）。
- **A: `session_send` を廃止**し `session_prompt` に統合。`mode`（`auto`（既定）/`queue`/`live`）を追加。
  - `auto` は `agent_state_store`（worktree 別 agent phase、ペイン終了時にクリア）で live pane を検知し queue/live を自動選択。
  - 返り値を `{name, delivered_to, detail}` にし、実際の配送チャネルを可視化。
  - `AgentBackend` に `agent_is_live(worktree) -> bool` を追加。mode→チャネルの判定は session サーバ側（テスト可能）に置き、本番 backend は phase ファイル読取のみ。

tool 数: 21 → 18（issue 6 + memory 5 + session 7）。

## 確認

- `cargo fmt` / `clippy -D warnings` / `test`（2700+ 件）green。
- ドキュメント更新: `document/03-commands/03-mcp.md`（tool 表・`session_prompt` 挙動・アーキ図・設計上の選択）、`01-cli.md` / `04-orchestration.md` / `data/01-global.md`（`session_send` アンカー修正）/ `data/README.md` / `02-architecture.md` / `README.md`。

## 補足

CLI の `issue list` / `memory list` は人間向けに存置（IF ごとに最適化する方針）。
