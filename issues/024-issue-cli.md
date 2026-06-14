---
number: 024
feature: issue-cli
title: issue CLI（`usagi issue` サブコマンド）
status: done
priority: high
category: cli
dependson: [023]
ref: PR #56
---

# `usagi issue` コマンド（タスク issue の CLI 操作）

## 概要

[023-issue-store](023-issue-store.md) の issue ストアを CLI から操作する `usagi issue` サブコマンド群を追加します。clap のサブコマンドとして実装し、人間が直接 issue を起票・更新・検索できるようにします。同じ usecase を [025-issue-mcp](025-issue-mcp.md) の MCP ツールからも再利用します。

## サブコマンド

| コマンド | 説明 |
|---|---|
| `usagi issue create` | issue を新規作成（`--title` / `--priority` / `--label` / `--body`、本文は `-` で stdin・未指定で `$EDITOR` 起動も検討） |
| `usagi issue list` | issue 一覧表示（`--status` / `--priority` / `--label` でフィルタ、既定は `updated_at` 降順） |
| `usagi issue show <number>` | 指定 issue の frontmatter + 本文を表示 |
| `usagi issue update <number>` | status / priority / title / labels / 本文の更新（`--status done` など） |
| `usagi issue search <query>` | タイトル・本文の全文検索（フィルタオプションと併用可） |
| `usagi issue delete <number>` | issue を削除（`--yes` なしの場合は確認） |

- 出力は人間向けの整形表示に加え、`--json` で機械可読出力に切り替え可能にする（MCP / スクリプト連携を見据える）。
- `usagi` がプロジェクトとして初期化されていない場合は分かりやすいエラーを出す。

## やること

- `presentation/cli` に `issue` サブコマンドのルーティングと各ハンドラを追加する。
- 023 の usecase（`create` / `update` / `list` / `search` / `delete` / `get`）を呼び出す薄い presentation 層として実装する。
- `--json` 出力フォーマットを定義する。
- `document/overview.md` の CLI コマンド表に `usagi issue` を追記する。
- `README.md` にユーザー向けの使い方を追記する。

## 完了条件

- 上記サブコマンドで issue の作成・一覧・表示・更新・検索・削除ができる。
- `--status` / `--priority` / `--label` のフィルタと全文検索が機能する。
- `--json` で機械可読な出力が得られる。
- カバレッジ 100% を維持する。
