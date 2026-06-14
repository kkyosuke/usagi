---
number: 025
feature: issue-mcp
title: issue MCP サーバ（`usagi mcp` で LLM に issue 操作を公開）
status: done
priority: high
category: mcp
dependson: [023, 024]
ref: PR #62
---

# `usagi mcp` コマンド（MCP サーバとして issue 操作を LLM に公開）

## 概要

usagi を MCP（Model Context Protocol）サーバとして起動し、LLM / AI エージェント（Claude Code 等）から issue を操作できるようにします。LLM は usagi の MCP ツールを通じて、作業対象プロジェクトのタスクを起票・更新・参照しながら開発を進められます。

stdio トランスポートで `usagi mcp` を起動し、エージェント側の MCP 設定（例: Claude Code の `mcpServers`）に登録して利用する想定です。

## 公開するツール

[023-issue-store](023-issue-store.md) の usecase をそのまま MCP ツールとして公開します。

| ツール | 説明 |
|---|---|
| `issue_create` | issue を作成（title / priority / labels / body） |
| `issue_update` | 指定 issue の status / priority / title / labels / body を更新 |
| `issue_list` | issue 一覧（status / priority / label フィルタ） |
| `issue_search` | タイトル・本文の全文検索（フィルタ併用可） |
| `issue_delete` | 指定 issue を削除 |
| `issue_get` | 指定 issue の詳細取得 |

- 各ツールの入出力スキーマを定義し、LLM が引数を正しく構成できる説明を付与する。
- 対象プロジェクト（`.usagi/issues/` の場所）の解決方法を決める（カレントディレクトリ / 環境変数 / 引数）。

## やること

- MCP サーバ実装用のクレート（Rust の MCP SDK、例: `rmcp`）を選定・導入する。技術スタック（tokio / serde）と整合させる。
- `presentation` に `usagi mcp` コマンド（stdio サーバ起動）を追加する。
- 023 / 024 と同じ usecase を呼ぶ MCP ツールハンドラを実装する（presentation 層に閉じる）。
- ツールのスキーマ・説明文を整備する。
- `document/overview.md` に MCP サーバとしての利用方法を追記する。
- `README.md` に AI エージェントからの利用手順（MCP 登録例）を追記する。

## 完了条件

- `usagi mcp` が stdio で MCP サーバとして起動する。
- MCP クライアント（Claude Code 等）から上記ツールで issue の CRUD・検索ができる。
- 操作結果が `.usagi/issues/` のファイルと index に正しく反映される。
- カバレッジ 100% を維持する。

> 依存: 本 issue は [023-issue-store](023-issue-store.md)（永続化基盤）と [024-issue-cli](024-issue-cli.md)（usecase の確立）の完了を前提とします。
