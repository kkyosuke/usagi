---
number: 018
feature: alias
title: usagi alias（コマンドエイリアス）
status: todo
priority: low
category: cli
dependson: [005]
ref: usagi.ai issue/alias.md
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
