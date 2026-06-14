---
number: 004
feature: space
title: space コマンド（ワークスペース切り替え）
status: todo
priority: high
category: tui
dependson: [002, 003]
ref: usagi.ai doc/app/tui/space.md
---

# `space` コマンド（ワークスペース切り替え）

## 概要

作成済みのセッション（worktree）間を切り替える TUI 内コマンドを実装します。現在アクティブなワークスペースを切り替えることで、後続のコマンド（`ai` / `terminal` / `diff` など）の実行対象が切り替わります。

## やること

- `space <name>` または一覧からの選択で、アクティブな worktree を切り替える。
- 現在アクティブな worktree をワークスペース画面上で視覚的に強調表示する。
- アクティブな worktree のパスを以降のコマンド実行のカレントディレクトリとして扱う。

## 完了条件

- 複数セッションがあるとき `space` で対象を切り替えられ、アクティブ表示が更新される。
- 切り替え後に実行する `terminal` / `ai` などが正しい worktree 配下で動作する。
