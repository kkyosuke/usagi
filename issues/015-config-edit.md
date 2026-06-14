---
number: 015
feature: config-edit
title: usagi config --edit（usagi.config の編集）
status: done
priority: medium
category: cli
dependson: [001]
ref: usagi.ai issue/config.md
---

# `usagi config --edit`

## 概要

`usagi.config` ファイルを対話的に、または既定のエディタで編集するコマンドを実装します。TUI の Config 画面（#019）はアプリ設定（通知 ON/OFF・Agent CLI 選択）が対象ですが、本 issue はプロジェクト単位の `usagi.config`（リポジトリ URL 等）を対象とします。

## やること

- `usagi config --edit` で `usagi.config` を既定エディタ（`$EDITOR`）で開く。
- 設定項目の一括表示と、対話的な値の修正に対応する。
- 保存時に形式チェック（必須項目・型）を行い、設定ミスを防ぐ。

## 完了条件

- `usagi config --edit` で `usagi.config` を編集・保存できる。
- 不正な形式の設定はエラーとして弾かれる。
