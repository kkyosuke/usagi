---
number: 63
title: fix: 軽微な correctness 改善（worktree_status の Err 握り潰し・remove dirty 判定の根拠明確化・new Mode の左右キー）
status: done
priority: low
labels: [fix, review]
dependson: []
related: []
created_at: 2026-06-19T22:17:16.381334+00:00
updated_at: 2026-06-19T22:17:16.381334+00:00
---

## 背景

実害は小さいが、将来バグになりうる/意図が読みにくい箇所をまとめて是正する。

- **`worktree_status` が Err を一律「git でない」と握り潰す**（`src/usecase/workspace_state.rs:61-67`）— not-a-git だけでなく git コマンドの一時失敗（lock 等）も同じ空状態に落ち、誤って `New`/`Local` に分類されうる。→ IO エラーと「git でない」を区別する。
- **`remove` の dirty 判定基準が二重**（`src/usecase/session/mod.rs:249-272`）— `record` 時に `WorktreeState.status` へ保存した dirty を使わず、`git::has_uncommitted_changes` を再 IO している。判定の正本が「保存値」か「実時間」か曖昧。→ 実時間で取り直す意図ならコメントで明記し、保存値の `status == Dirty` 表現との関係を整理する。
- **new 画面の Mode で ←/→ が別アーム同一処理**（`src/presentation/tui/new/event.rs:74-81`）— どちらも `toggle_mode()` を呼ぶ。2 値モードでは正しいが 3 値以上になると破綻。→ `Key::ArrowLeft | Key::ArrowRight if focus==Mode` の 1 アームに畳むか、方向付き cycle にする。

## 確認方法

- 各分岐の意図がコードから読み取れること。
- 既存テストが通ること（カバレッジ 100% 維持）。
