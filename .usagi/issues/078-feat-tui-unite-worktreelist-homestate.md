---
number: 78
title: feat(tui): unite基盤 — WorktreeList/HomeState をグループ対応データモデルへ一般化
status: in-progress
priority: medium
labels: [feat, tui, refactor]
dependson: []
related: []
parent: 77
created_at: 2026-06-28T00:08:09.516046+00:00
updated_at: 2026-06-28T00:09:06.696040+00:00
---

統合(unite)モードの基盤（親 #77 のフェーズ1）。

## 方針

`WorktreeList` の内部を「ワークスペース・グループの配列」を横断する flat な選択行空間に書き換える。**本番は単一グループのまま（挙動不変・既存テストは緑）**、複数グループは単体テストで実証する。

- 新 value 型 `WorkspaceGroup { name, root_path, root_has_note, worktrees, labels, notes }` を導入。`WorktreeList { groups: Vec<WorkspaceGroup>, selected_index, active_index, previous_active }`。
- 選択行空間 = 各グループの [root, wt0, wt1, …] を連結したもの。行 → `(group_index, Option<worktree_index>)` を解決。
- `move_up/down`・`selected/active`・`refs`・`select_by_name`・`activate_by_name`・`session_count` をグループ横断で動くよう書き換え（単一グループでは現挙動と完全一致）。
- `previous_active`(Ctrl-^) を **(グループ root_path, name)** で修飾し、同名セッションが別グループにあっても取り違えないようにする。
- `root_path`/`root_note` を `WorkspaceGroup` 側へ移し、HomeState からはカーソル/アクティブ行のグループで解決する（既存の `set_root_path`/`root_path()` は単一グループに委譲して互換維持）。
- 既存 public API はできるだけ維持し、コンパイルを壊さず段階移行する。

## 確認方法

- 既存の state/list/event テストが全て緑（単一グループの挙動不変）。
- 2 グループを直接構築する新規単体テスト: カーソルのグループ横断移動、グループ別 root 行、同名セッションの取り違え無し、`refs`/`session_count` の合計。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100%）。
