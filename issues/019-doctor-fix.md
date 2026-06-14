---
number: 019
feature: doctor-fix
title: usagi doctor --fix（依存関係の自動修復）
status: done
priority: medium
category: cli
dependson: []
ref: usagi.ai issue/doctor-fix.md
---

# `usagi doctor --fix`

## 概要

`usagi doctor` を拡張し、不足している依存関係を自動修復、またはインストール方法を提示する `--fix` オプションを実装します。既存の doctor（依存・通知・設定ストレージのヘルスチェック）の結果を踏まえ、環境構築の手間を最小化します。

## やること

- `git` / `bash` / `node` 等の不足ツールを検出して提示する。
- OS に合わせたパッケージマネージャ（brew / apt / cargo 等）でのインストールを試行する。
- 自動修復できないものは、手動インストール手順を提示する。

## 完了条件

- `usagi doctor --fix` で不足ツールのインストールが試行される。
- 修復不可の場合は具体的な手順が表示される。
