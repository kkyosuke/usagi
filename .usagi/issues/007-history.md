---
number: 7
title: history コマンド（コマンド実行履歴の表示）
status: done
priority: medium
labels: [tui]
dependson: [2]
related: []
created_at: 2026-06-16T23:00:12.312491+00:00
updated_at: 2026-07-02T22:14:43.272033+00:00
---

# `history` コマンド（コマンド実行履歴の表示）

## 概要

各ワークスペースで実行したコマンドの履歴を TUI 内で閲覧するコマンドを実装します。`.usagi/history.json` に蓄積された履歴を一覧表示し、後から作業内容を振り返れるようにします。

## やること

- `history` で `.usagi/history.json` の内容を時系列に一覧表示する。
- セッション（worktree）ごとに履歴をフィルタできるようにする。
- 実行時刻・コマンド・結果（成功/失敗）を併記する。

## 完了条件

- これまで実行したコマンドが `history` で時系列に表示される。
- セッション単位で履歴を絞り込める。
