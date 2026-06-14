---
number: 014
feature: clean
title: usagi clean（古いセッションの整理）
status: todo
priority: low
category: cli
dependson: [003]
ref: usagi.ai issue/clean.md
---

# `usagi clean`

## 概要

長期間放置された不要なセッション（worktree）や古い状態データを一括でクリーンアップするコマンドを実装します。ディスク容量の節約とプロジェクトのクリーンな状態維持を支援します。

## やること

- 最終更新から一定期間経過したセッション（worktree）を検索し、削除を提案する。
- 重複した一時ファイルや `.usagi/` 内の古い状態データを整理する。
- 削除前に対象を一覧表示し、確認（dry-run / 対話確認）してから実行する。

## 完了条件

- `usagi clean` で放置セッションが検出され、確認のうえ削除できる。
- `--dry-run` で削除対象だけを表示できる。
