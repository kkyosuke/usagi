---
number: 5
title: ai コマンド（AI エージェントへの指示・対話）
status: todo
priority: high
labels: [tui]
dependson: [2]
related: []
created_at: 2026-06-16T22:59:17.924160+00:00
updated_at: 2026-06-16T22:59:17.924160+00:00
---

# `ai` コマンド（AI エージェントへの指示・対話）

## 概要

アクティブなワークスペースで AI エージェント CLI を起動・対話する TUI 内コマンドを実装します。Config 画面（#019 で実装済み）で選択された Agent CLI（Claude Code 等）を利用し、現在の worktree をコンテキストとして AI に指示を渡します。

## やること

- `ai <prompt>` で選択中の Agent CLI を起動し、プロンプトを渡す。
- Config の Agent CLI 選択設定を参照して起動コマンドを決定する。
- アクティブな worktree をカレントディレクトリとして AI を実行する。
- やり取りを履歴に記録する。

## 完了条件

- Config で選択した Agent CLI が `ai` から起動できる。
- worktree 配下のファイルが AI のコンテキストとして扱われる。
- Agent CLI 未選択時は分かりやすいエラーを表示する。
