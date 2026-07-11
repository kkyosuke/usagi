---
number: 188
title: perf(test): Git fixture の repository/worktree 構築コストを削減する
status: todo
priority: medium
labels: [perf, test]
dependson: []
related: [181]
parent: 177
created_at: 2026-07-11T00:41:58.711256+00:00
updated_at: 2026-07-11T00:41:58.711256+00:00
---

## 背景

issue #181 の nextest duration 計測で `usecase::update` を中心とする Git repository/session/worktree fixture が slow 上位を占めた。`distributes_the_default_branch...` は 8.63s、`resolves_the_workspace_root...` は 4.45s だった。

## 対応

- 共通の bare repository / commit graph fixture を再利用できる範囲を特定する
- test ごとの isolation と順序非依存を維持する
- subprocess 回数と wall time を変更前後で計測する

## 完了条件

対象 test 群の wall time を意味のある幅で短縮し、full suite と coverage 100% を維持する。
