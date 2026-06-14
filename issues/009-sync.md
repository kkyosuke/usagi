---
number: 009
feature: sync
title: usagi sync（main の変更をセッションへ同期）
status: todo
priority: medium
category: cli
dependson: [003]
ref: usagi.ai issue/sync.md
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
