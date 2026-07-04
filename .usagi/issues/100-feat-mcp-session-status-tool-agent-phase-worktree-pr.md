---
number: 100
title: feat(mcp): session_status tool — agent phase・worktree 状態・PR 状態を公開しコーディネータが完了検知できるようにする
status: todo
priority: high
labels: [orchestration, mcp]
dependson: []
related: []
created_at: 2026-07-04T05:09:42.602913+00:00
updated_at: 2026-07-04T05:09:42.602913+00:00
---

## 背景

コーディネータ役のエージェント（root 行の agent）が委譲先セッションの進捗を知る手段が MCP にない。agent phase（`ready` / `running` / `waiting` / `ended`）は `~/.usagi/agent-state/` に記録済みで TUI バッジを駆動しているが MCP から読めず、`session_pr` は PR の URL だけで open / merged / closed の状態を含まない。このため「子が終わった」「PR がマージされた」を検知して次の issue に進む自律ループが閉じない。

## やること

- MCP に `session_status`（名前は要検討。`session_list` の拡張でも可）を追加し、セッションごとに次を返す:
  - agent phase（`~/.usagi/agent-state/` の worktree 別記録。ペインが無い場合は `none` など）
  - 各 worktree の `status`（`local` / `pushed` / `merged`。`usagi status` と同じ同期ロジック）
  - dirty（未コミット変更）の有無
- `session_pr` の返り値に PR の状態（open / merged / closed）を含める（`gh` 依存にするか、worktree の merged 判定で代替するかは実装時に判断）。
- 読み取り専用・軽量にする（呼ぶたびの重い git spawn を避ける工夫。既存の status 同期キャッシュを流用）。

## 受け入れ条件

- コーディネータが `session_status` のポーリングだけで「子エージェント完了（ended）」「PR マージ済み（merged）」を判定し、`session_remove` → 次の issue 委譲へ進める。
- ドキュメント（03-mcp）を更新する。
