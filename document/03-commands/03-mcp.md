# 3.3 MCP サーバ（`usagi mcp`）

> [コマンドリファレンス](README.md) ｜ ← 前へ [3.2 TUI 内コマンド](02-tui.md) ｜ 次へ → [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md)

`usagi mcp` は、`usagi issue`（[01-cli.md](01-cli.md#usagi-issue)）と同じタスク issue 操作を
**MCP（Model Context Protocol）サーバ**として AI エージェントに公開するコマンドです。エージェント
（Claude Code など）が tool 呼び出しでタスクを起票・参照・更新できます。

## 目次

- [概要](#概要)
- [起動と登録](#起動と登録)
- [アーキテクチャ](#アーキテクチャ)
- [対応 tool 一覧](#対応-tool-一覧)
- [JSON-RPC プロトコル](#json-rpc-プロトコル)
- [エラーハンドリング](#エラーハンドリング)
- [設計上の選択](#設計上の選択)

## 概要

- **トランスポート**: stdio（標準入出力）上の **JSON-RPC 2.0**。1 メッセージ = 1 行の JSON。
- **対象リポジトリ**: `usagi mcp` を起動したカレントディレクトリの `.usagi/issues/`。
- **ロジックの共有**: 各 tool は CLI と同じ [`usecase/issue`](../02-architecture.md#各層の責務) を呼ぶ薄い
  アダプタ。CLI と MCP で挙動（採番・依存 readiness 判定など）は完全に一致します。

## 起動と登録

シェルから直接起動できますが、通常は MCP クライアント（エージェント）に登録して使います。

```bash
usagi mcp   # stdin から JSON-RPC を読み、stdout へ応答を書く
```

Claude Code への登録例（対象プロジェクト直下で起動させる想定）:

```json
{
  "mcpServers": {
    "usagi": { "command": "usagi", "args": ["mcp"] }
  }
}
```

手元での動作確認（パイプで 1 リクエストを渡す）:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | usagi mcp
```

## アーキテクチャ

MCP 層は「プロトコルの解釈」と「stdio の入出力」を分離し、ビジネスロジックは既存層へ委譲します。

```
AIエージェント ⇄ (stdio JSON-RPC)
        │
        ▼
presentation/cli/mcp.rs   … stdin ループ（薄い I/O ラッパ。カバレッジ対象外）
        │  handle_line(line) ごとに委譲
        ▼
presentation/mcp.rs       … McpServer：JSON-RPC ディスパッチ + tool 実装（100% テスト）
        │  各 tool が呼ぶ
        ▼
usecase/issue             … create / get / list / search / update / delete・readiness 判定
        │
        ▼
infrastructure/issue_store … <repo>/.usagi/issues/ の markdown + index.json
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/mcp.rs` | `usagi mcp` のエントリ。stdin を 1 行ずつ読み、`McpServer::handle_line` の戻り値を stdout へ書く。ブロッキング I/O のみで、`hop` 同様カバレッジ計測の対象外。 |
| `presentation/mcp.rs` | `McpServer`。`handle_line(&str) -> Option<String>` を純粋関数として実装（副作用は usecase 経由のファイル操作のみ）。プロトコル分岐・tool ディスパッチ・JSON 整形を担う。ユニットテストで網羅。 |
| `usecase/issue` ほか | tool が呼ぶビジネスロジック。MCP 固有の知識は持たない。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase → infrastructure`。MCP 層は
presentation に閉じています（[2. アーキテクチャ](../02-architecture.md) 参照）。

## 対応 tool 一覧

`tools/list` で以下の 6 tool を公開します。結果はいずれも JSON テキストで返ります。

| tool | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `issue_create` | `title` | `priority` / `labels` / `dependson` / `body` | 作成された issue |
| `issue_get` | `number` | — | issue（存在しなければ `null`） |
| `issue_list` | — | `status` / `priority` / `label` / `ready` | issue 配列（各要素に `ready` と `unmet_deps` を付与） |
| `issue_search` | `query` | `status` / `priority` / `label` / `ready` | 一致した issue 配列（`list` と同形式） |
| `issue_update` | `number` | `title` / `status` / `priority` / `labels` / `dependson` / `body` | 更新後の issue |
| `issue_delete` | `number` | — | `{ "number": N, "deleted": bool }` |

- `status` は `todo` / `in-progress` / `done`、`priority` は `high` / `medium` / `low`。
- `issue_list` / `issue_search` は CLI と同じく **`dependson` がすべて `done` の issue を `ready: true`**
  とし、未達の依存番号を `unmet_deps` に入れて返します（着手可能なタスクの判別用）。
- `ready: true`（引数）を渡すと着手可能な issue だけに絞り込みます。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## JSON-RPC プロトコル

実装するのは MCP の最小サブセットです。各メッセージは改行区切りの JSON で、`id` を持つものが
リクエスト（要応答）、持たないものが通知（応答不要）です。

### `initialize`

```json
→ {"jsonrpc":"2.0","id":1,"method":"initialize"}
← {"jsonrpc":"2.0","id":1,"result":{
     "protocolVersion":"2024-11-05",
     "capabilities":{"tools":{}},
     "serverInfo":{"name":"usagi","version":"<crate version>"}}}
```

### `tools/list`

公開 tool とその `inputSchema` の配列を返します。

```json
→ {"jsonrpc":"2.0","id":2,"method":"tools/list"}
← {"jsonrpc":"2.0","id":2,"result":{"tools":[ { "name":"issue_create", "description":"…", "inputSchema":{…} }, … ]}}
```

### `tools/call`

`params.name` で tool を指定し、`params.arguments`（省略時は空オブジェクト）を渡します。

```json
→ {"jsonrpc":"2.0","id":3,"method":"tools/call",
   "params":{"name":"issue_create","arguments":{"title":"ログイン画面","priority":"high","dependson":[1]}}}
← {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{ …作成された issue の JSON… }"}],"isError":false}}
```

### `ping`

`{}` を返します。

### 通知

`notifications/initialized` などの通知（`id` なし）は受理しますが、応答は返しません。

## エラーハンドリング

エラーは 2 種類に分けて扱います。

- **プロトコルエラー**: JSON-RPC の `error` オブジェクトで返します。
  | code | 状況 |
  |---|---|
  | `-32700` | パースエラー（不正な JSON） |
  | `-32600` | `method` の無い不正なリクエスト |
  | `-32601` | 未知のメソッド |
  | `-32602` | `tools/call` に tool 名が無い |
- **tool 実行エラー**: `tools/call` の結果として `isError: true` を立てて返します（プロトコルエラーには
  しません）。これによりエージェントがエラー内容をテキストで受け取り、自己修復できます。
  - 例: 不正な引数（必須項目の欠落・型不一致）、未知の tool 名、`issue_update` の対象が存在しない。

## 設計上の選択

- **自前実装（依存追加なし）**: MCP の SDK（`rmcp` 等）は tokio など非同期スタックを要しますが、本
  サーバは `serde_json` のみで同期的に実装しています。usagi の「依存を最小に保つ」「テストカバレッジ
  100%」という方針に合わせ、protocol 分岐を純粋関数（`handle_line`）に閉じ込めてユニットテストで
  網羅し、テスト不能な stdin ループだけをカバレッジ対象外にしています。
- **protocolVersion**: `2024-11-05` を返します。
- **状態を持たない**: サーバはセッション状態を保持せず、各 tool 呼び出しが `.usagi/issues/` を直接
  読み書きします。CLI と MCP を混在して使っても整合します。
