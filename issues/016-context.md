---
number: 016
feature: context
title: usagi context（AI 用コンテキスト生成）
status: todo
priority: low
category: cli
dependson: [001]
ref: usagi.ai issue/context.md
---

# `usagi context`

## 概要

AI エージェントに読み込ませるための「プロジェクトのコンテキスト」を生成するコマンドを実装します。プロンプト用に最適化されたプロジェクト概要を出力し、`ai` コマンド（#005）や外部エージェントへの入力として利用します。

## やること

- ファイルツリーやプロジェクトの主要な構成情報を整形して出力する。
- 重要なファイルの抜粋や最近の `git diff` を含めた要約を生成する。
- 出力フォーマット（Markdown / プレーン）を選べるようにする。

## 完了条件

- `usagi context` でプロジェクト概要が整形出力される。
- 出力が `ai` コマンドや外部エージェントの入力として利用できる。
