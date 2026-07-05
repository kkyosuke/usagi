---
number: 17
title: usagi init-agent（AI エージェント設定ファイルの初期化）
status: done
priority: low
labels: [cli]
dependson: [1]
related: []
created_at: 2026-06-16T23:02:04.449084+00:00
updated_at: 2026-07-05T22:21:58.439472+00:00
---

# `usagi init-agent`

## 概要

AI エージェント用の設定ファイルを、プロジェクトの構成に合わせて初期化するコマンドを実装します。既存プロジェクトに AI エージェントのコンテキストをすばやく導入できるようにします。

## やること

- `.aider.conf.yml` / `.clinerules` / `CLAUDE.md` などのエージェント設定ファイルを自動生成する。
- プロジェクトの言語・フレームワーク等を検出し、推奨ルールを初期設定する。
- 既存ファイルがある場合は上書き確認を行う。

## 完了条件

- `usagi init-agent` で対象エージェントの設定ファイルが生成される。
- プロジェクト構成に応じた初期内容が書き込まれる。
