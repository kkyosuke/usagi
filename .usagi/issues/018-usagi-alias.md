---
number: 18
title: usagi alias（コマンドエイリアス）
status: done
priority: low
labels: [cli]
dependson: [5]
related: []
created_at: 2026-06-16T23:02:11.589215+00:00
updated_at: 2026-06-16T23:02:11.589215+00:00
---

# `usagi alias`

## 概要

複雑な AI エージェントの起動コマンド等に短い名前（エイリアス）を設定する機能を実装します。プロジェクトごとにエージェントの呼び出し方を切り替えやすくします。

## やること

- `aider --model sonnet --no-stream` のような長いコマンドに名前を付けて保存する。
- 保存したエイリアスを TUI（コマンドモード / `ai` コマンド）からワンステップで呼び出す。
- プロジェクト単位でエイリアスを管理する（`usagi.config` または `.usagi/` に保存）。

## 完了条件

- `usagi alias <name> <command>` でエイリアスを登録できる。
- 登録したエイリアスを TUI から呼び出して実行できる。
