---
number: 316
title: chore(tui): 旧 Workspace view 経路を削除しテストを controller 経路へ統合する
status: todo
priority: high
labels: [tui, cleanup]
dependson: [315]
related: [258]
parent: 258
created_at: 2026-07-17T14:22:28.395887+00:00
updated_at: 2026-07-17T14:22:28.395887+00:00
---

## 目的

#258 の最終段階。runtime 切替（#315）後に残る旧 `Workspace` view の row state・駆動関数・render を削除し、二重定義を完全に解消する。#315 の直後に最短で行い、二重実装の併存期間を最小化する。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.6 / §5 PR4。

## スコープ

- 旧 `Workspace` view の row state 一式を削除する: `selected: usize`、`root_selected` / `new_session_selected` / `focused_session` / `pane_target` / `row_count` / `select_next/prev` / `selectable_rows` / `workspace_viewport_start` / `sidebar_row_at`、`mode`、`create_input` / `pending_session`、`panes` / `pane_documents`（session 名キー投影）。
- `step_switch` / `step_closeup` / `step_closeup_tabs` / `apply_live_action` 等、旧 view を駆動する自由関数を削除する（reducer に同等あり）。
- `workspace::render` / `render_with_skeleton_frame` と `skeleton_frame` カウンタを削除する。
- 旧 render 系テストを削除または `render_home` 系へ統合する。
- `document/`（02-architecture の TUI 経路記述）と必要なら `README.md` を更新する。
- #258 の status を `done` にするコミットを同じ PR に含める（PR を開く前）。

## 完了条件

- 旧 `Workspace` の sessions/row state が実 runtime の source of truth として残らない（#258 完了条件）。
- full test + coverage 100%（削除でカバレッジ欠けが出ない）。Markdown link check を通す。
