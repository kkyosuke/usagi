---
number: 9
title: usagi sync（main の変更をセッションへ同期）
status: done
priority: medium
labels: [cli]
dependson: [3]
related: []
created_at: 2026-06-16T23:00:27.664183+00:00
updated_at: 2026-06-16T23:00:27.664183+00:00
---

# `usagi sync`

## 概要

メインブランチの最新の変更を、現在のセッションに取り込むコマンドを実装します。並行して作業中の複数セッションのベースを最新に保ちやすくします。

## やること

- origin のデフォルトブランチから最新を `fetch` する。
- 現在のセッション（worktree）に対して `rebase` または `merge` を行う（方式は選択可能にする）。
- コンフリクト発生時は分かりやすく通知し、解決を促す。

## 完了条件

- `usagi sync` で main の最新コミットがアクティブセッションに取り込まれる。
- コンフリクト時に処理を中断し、状況を明示する。
