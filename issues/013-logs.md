---
number: 013
feature: logs
title: usagi logs（履歴の閲覧・検索）
status: todo
priority: low
category: cli
dependson: [007]
ref: usagi.ai issue/logs.md
---

# `usagi logs`

## 概要

各セッションでの履歴情報を一括で閲覧・検索するコマンドを実装します。TUI 内の `history`（#007）が表示する情報を、ターミナルからも横断的に検索できるようにします。

## やること

- 各 worktree（セッション）で実行されたコマンド履歴を横断検索する。
- AI エージェントとのやり取りの履歴（Agent CLI のログ等）を集約して俯瞰する。
- キーワード / 期間 / セッションでの絞り込みに対応する。

## 完了条件

- `usagi logs <keyword>` で全セッションの履歴を横断検索できる。
- 期間やセッションでフィルタできる。
