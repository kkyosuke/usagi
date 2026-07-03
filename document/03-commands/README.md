# 3. コマンドリファレンス

> [ドキュメント目次](../README.md) ｜ ← 前へ [2. アーキテクチャ](../02-architecture.md) ｜ 次へ → [4. オーケストレーション](../04-orchestration.md)

`usagi` のコマンドは **CLI コマンド**（シェルから `usagi <cmd>` で実行）と **TUI 内コマンド**
（`usagi hop` 起動後、ホーム画面のコマンドラインで実行）の 2 系統に分かれます。本ディレクトリでは
系統ごとにファイルを分けて一覧と詳細をまとめます。

## 目次

| # | ドキュメント | 内容 |
|---|---|---|
| 1 | [01-cli.md](01-cli.md) | CLI コマンド（シェルから `usagi <cmd>` で実行）の一覧と詳細 |
| 2 | [02-tui.md](02-tui.md) | TUI 内コマンド（`usagi hop` のホーム画面で実行）の一覧 |
| 3 | [03-mcp.md](03-mcp.md) | MCP サーバ（`usagi mcp`）。issue・memory・session の tool を 1 サーバで公開。アーキテクチャ・対応 tool・プロトコル |
| 4 | [04-llm-mcp.md](04-llm-mcp.md) | ローカル LLM MCP サーバ（`usagi llm-mcp`）。Agent のトークン消費を抑える委譲ツール |
