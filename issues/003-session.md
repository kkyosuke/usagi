---
number: 003
feature: session
title: session コマンド（セッション管理）
status: todo
priority: high
category: tui
dependson: [002]
ref: usagi.ai doc/app/tui/session.md
---

# `session` コマンド（セッション管理）

## 概要

セッション（ブランチ + worktree のペア）を作成・管理する TUI 内コマンドを実装します。usagi の worktree ベースワークフローの中心機能であり、`space` / `sync` / `finish` / `list` / `clean` / gh 連携など多くの機能がこのセッション概念に依存します。

## やること

- `session new <name>`：新しいブランチを切り、対応する worktree を作成する。
- `session list`：現在のプロジェクトのセッション一覧を表示する。
- `session remove <name>`：セッション（worktree + ブランチ）を削除する。
- セッション情報（ブランチ名・worktree パス・ベースブランチ・作成時刻）を `.usagi/state.json` に永続化する。
- ワークスペース画面の worktree 一覧ペインにセッションを反映する。

## 完了条件

- `session new feature-x` で `feature-x` ブランチと worktree が作成され、一覧に表示される。
- `session remove feature-x` で worktree とブランチが安全に削除される（未コミット変更がある場合は警告）。
- セッション状態が再起動後も `state.json` から復元される。
