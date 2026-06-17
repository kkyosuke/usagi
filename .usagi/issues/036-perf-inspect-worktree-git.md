---
number: 36
title: perf: inspect_worktree の git プロセス起動を削減する
status: todo
priority: high
labels: [perf, core]
dependson: []
related: []
created_at: 2026-06-17T22:50:04.342428+00:00
updated_at: 2026-06-17T22:50:04.342428+00:00
---

## 背景

コードレビューで判明したパフォーマンス問題。最も体感影響が大きい。

`inspect_worktree`（`src/usecase/workspace_state.rs:49-64`、`src/infrastructure/git/branch.rs`）は 1 worktree あたり 6〜8 回 git プロセスを起動している。

- `worktree_head` … `rev-parse HEAD` と `rev-parse --abbrev-ref HEAD` の 2 プロセス
- `default_branch` … `symbolic-ref refs/remotes/origin/HEAD`、失敗時さらに `rev-parse --abbrev-ref HEAD`
- `upstream_of` … `rev-parse ...@{upstream}`
- `has_uncommitted_changes` … `status --porcelain`
- `ahead_behind` … `rev-parse origin/<into>` + `rev-list --left-right --count`

`sync`（`workspace_state.rs:29-34`）は home 画面に入るたび・`reload_sessions` のたび・`usagi status` のたびに全 session × 全 worktree で走るため、N worktree で **6N〜8N 回の fork/exec が画面表示のたびに発生**する。rayon 並列化済みだがプロセス数自体は減らない。

## 改善方針

- `git status --porcelain=v2 --branch` 1 回で branch / upstream / ahead-behind / dirty をまとめて取得する。
- `default_branch` はリポジトリ単位なので worktree ごとに引き直さず、`sync` の外で 1 回解決して各 worktree に渡す。
- `rev-parse HEAD --abbrev-ref HEAD` は 1 プロセスに統合する。

## 確認方法

- worktree 数を増やして `usagi status` / home 画面表示時の git プロセス起動回数が削減されていること。
- 既存テストが通ること（カバレッジ 100% 維持）。
