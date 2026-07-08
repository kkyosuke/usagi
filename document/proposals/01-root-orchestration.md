# 提案（畳み込み済み）: 自律オーケストレーション運用モデル

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

この提案の内容は Epic #105 の実装により正本ドキュメントへ畳み込み済みです。現在の仕様は次を参照してください。

| 内容 | 正本 |
|---|---|
| root と session の責務分界 / 起源フロー / status ライフサイクル / ガードレール | [04-orchestration.md#自律オーケストレーション運用モデル](../04-orchestration.md#自律オーケストレーション運用モデル) |
| MCP tool の一覧・`session_delegate_brief` / `session_delegate_issue` の挙動・root 書き込みガード | [03-commands/03-mcp.md](../03-commands/03-mcp.md) |
| AI エージェントの作業フロー・status 単一書き手規約 | [../../.agents/workflow.md](../../.agents/workflow.md) |
| pre-commit backstop と開発規約 | [06-conventions.md](../06-conventions.md) |

設計の履歴は git の履歴で確認します。このファイルは古いリンクから正本へ案内するために残しています。
