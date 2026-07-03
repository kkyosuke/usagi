# 3.5 1Password MCP サーバ（`usagi op-mcp`）

> [コマンドリファレンス](README.md) ｜ ← 前へ [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md)

`usagi op-mcp` は、[1Password CLI（`op`）](https://developer.1password.com/docs/cli/)を
**MCP（Model Context Protocol）サーバ**として AI エージェントに公開するコマンドです。
エージェントは、タスクに必要な資格情報（API キー・トークン・接続文字列など）を
**その場で 1Password から読み取れる**ため、秘密情報をプロンプトに貼り付けたり
リポジトリにコミットしたりせずに済みます。公開する tool は**読み取り専用**で、
秘密情報の書き込み・削除・変更は行いません。

## 目次

- [概要](#概要)
- [有効化（OS のシークレットストアに保存）](#有効化os-のシークレットストアに保存)
- [前提（認証）](#前提認証)
- [起動と登録](#起動と登録)
- [対応 tool 一覧](#対応-tool-一覧)
- [アーキテクチャ](#アーキテクチャ)
- [エラーハンドリング](#エラーハンドリング)
- [設計上の選択](#設計上の選択)

## 概要

- **トランスポート**: stdio 上の **JSON-RPC 2.0**（[MCP サーバ](03-mcp.md) と同じ実装）。
- **バックエンド**: 各 tool 呼び出しが `op` を 1 回だけ実行し、標準出力を返す。標準入力は
  閉じて実行し、応答が返らないまま固まらないようタイムアウトで打ち切る。
- **読み取り専用**: 公開するのは参照系の操作（secret reference の解決・item / vault の参照）だけ。
  item の作成・編集・削除や `op run` のような任意コマンド実行は公開しない。
- **状態を持たない**: サーバは資格情報を保持せず、認証は `op` 側（環境）に委ねる。

## 有効化（OS のシークレットストアに保存）

このサーバは**既定では wire されません**。`usagi op login` で **1Password サービスアカウントトークン**を
OS のシークレットストア（macOS Keychain / Linux Secret Service）に保存すると、エージェントへ自動 wire されます
（[5. 設定](../05-settings.md) 参照）。

| コマンド / 設定 | 役割 |
|---|---|
| `usagi op login` | トークンを OS シークレットストアに保存し、`op_mcp.enabled = true` にする |
| `usagi op logout` | トークンを OS シークレットストアから削除し、`op_mcp.enabled = false` にする |
| `usagi op status` | `op_mcp.enabled` と、OS シークレットストアにトークンが存在するかを表示する |
| `op_mcp.enabled` | `agent` 起動時に `usagi-op` サーバを wire する非 secret な設定フラグ（トークン本体ではない） |

登録:

```bash
usagi op login
```

- `usagi op login` はトークンをエコーしない入力で受け取り、OS シークレットストアに保存します。`settings.json` には
  トークン本体を書きません。
- `op_mcp.enabled` が `true` のとき、Claude は `--mcp-config`、Codex は `-c mcp_servers.usagi-op.*` 設定上書きで
  `usagi-op` サーバ（`usagi op-mcp`）が起動コマンドに追加されます（Gemini はインライン注入に対応しないため wire されません）。
  Codex / `codex-fugu` では `mcp_servers.usagi-op.default_tools_approval_mode = "approve"` も渡すため、`usagi-op`
  の tool 呼び出しごとの確認は省かれます。
- トークンは **`usagi op-mcp` プロセスが OS シークレットストアから読み取り**、`op` サブプロセスへ環境変数
  `OP_SERVICE_ACCOUNT_TOKEN` として渡します。**エージェントの起動コマンド行やプロセス一覧には出ません**。
- `usagi config`（表示）は `op_mcp.enabled` だけを出力します。実際にトークンが保存されているかは
  `usagi op status` で確認します。

> **セキュリティ注記**: サインイン済みなら、エージェントはこのトークンで読める secret をすべて参照できます。

## 前提（認証）

このサーバは `op` を呼び出すだけで、認証そのものは `op`（環境）に委ねます。次のいずれかが必要です。

- **OS シークレットストアに登録（推奨・自動 wire の前提）**: 上記 `usagi op login`。`usagi op-mcp` が
  これを読み、`op` へ `OP_SERVICE_ACCOUNT_TOKEN` として渡す。
- **対話的**: `op signin`（1Password デスクトップアプリ連携、または `eval $(op signin)`）。手動登録で
  サーバを直接起動する場合に使える（自動 wire はトークン登録が前提）。
- **非対話的（環境変数）**: シェルで `OP_SERVICE_ACCOUNT_TOKEN` を export 済みなら、`op` がそれを使う。

未認証のまま tool を呼ぶと、`op` のエラー（例: `no active session found`）が
tool 実行エラー（`isError: true`）としてそのまま返ります。

## 起動と登録

通常は上記のとおり **`usagi op login` でトークンを保存すれば usagi が自動で wire** します。手元での確認や、usagi 以外の
MCP クライアントから使う場合はシェルから直接起動・登録もできます。

```bash
usagi op-mcp   # stdin から JSON-RPC を読み、stdout へ応答を書く
```

usagi 以外の MCP クライアントへ手動登録する例:

```json
{
  "mcpServers": {
    "usagi-op": { "command": "usagi", "args": ["op-mcp"] }
  }
}
```

手元での動作確認（パイプで 1 リクエストを渡す）:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | usagi op-mcp
```

## 対応 tool 一覧

`tools/list` で以下の 5 tool を公開します。結果はいずれも `op` の出力（多くは JSON テキスト）を返します。

| tool | 必須引数 | 任意引数 | 返り値 | 実行する `op` |
|---|---|---|---|---|
| `op_read` | `reference` | — | secret reference の値 | `op read --no-newline <reference>` |
| `op_item_get` | `item` | `vault` / `fields` | item の JSON | `op item get <item> --format json [--vault …] [--fields …]` |
| `op_item_list` | — | `vault` | item 配列の JSON（メタデータのみ） | `op item list --format json [--vault …]` |
| `op_vault_list` | — | — | vault 配列の JSON | `op vault list --format json` |
| `op_whoami` | — | — | サインイン中アカウントの JSON | `op whoami --format json` |

- `reference` は `op://<vault>/<item>/<field>` 形式の secret reference。`--no-newline` を付けるため、
  値は末尾の改行なしで返ります。
- `op_item_get` の `fields` は `username,password` のようなカンマ区切りの取得対象フィールド。
  `vault` / `fields` は与えても**空白だけなら省略扱い**にし、空のオペランドを `op` に渡しません。
- 必須の `reference` / `item` が空（空白のみ）の場合は、`op` を実行せず tool 実行エラーで即座に返します。
- `op_item_list` / `op_vault_list` はメタデータのみを返し、secret の値は含みません。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## アーキテクチャ

```
AIエージェント ⇄ (stdio JSON-RPC)
        │
        ▼
presentation/cli/op_mcp.rs   … stdin ループ + op バックエンドの注入（ストリーム注入でテスト）
        │  handle_line(line) ごとに委譲
        ▼
presentation/mcp/op.rs       … OpMcpServer：tool 実装と引数→`op` 引数の組み立て（JSON-RPC フレーミングは mcp/mod.rs と共有・100% テスト）
        │  OpBackend::run(args) 経由
        ▼
（テスト時）FakeBackend / （本番）OpCliBackend → `op <args>`
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/op_mcp.rs` | `usagi op-mcp` のエントリ。stdin ループと `op` バックエンドの注入。`mcp` / `llm-mcp` 同様、本番バックエンドを合成ルート（`main.rs`）で束ね、ストリームを注入してユニットテストで確認する。 |
| `presentation/mcp/op.rs` | `OpMcpServer`。`McpService` を実装し 5 つの読み取り tool を提供（JSON-RPC フレーミングは `mcp/mod.rs` と共有）。各 tool の引数を `op` の引数列へ組み立てる純ロジックを `OpBackend` トレイトの先に閉じ込め、ユニットテストで網羅。 |
| `main.rs`（合成ルート） | 本番 `OpBackend`（`OpCliBackend`）。`op` をサブプロセスで 1 回実行し、stdout を取得・stderr を診断に使う。実 IO のみのためカバレッジ対象外（[06-conventions.md](../06-conventions.md#品質チェックコミットpush-前に必須)）。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase → infrastructure`。MCP 層は
presentation に閉じています（[2. アーキテクチャ](../02-architecture.md) 参照）。

## エラーハンドリング

[MCP サーバ](03-mcp.md#エラーハンドリング)と同じ方針です。

- **プロトコルエラー**: 不正な JSON（`-32700`）・`method` 欠落（`-32600`）・未知メソッド（`-32601`）・
  tool 名欠落（`-32602`）は JSON-RPC の `error` で返します。
- **tool 実行エラー**: 引数不備（必須項目の欠落・空）、未知の tool 名、`op` の非ゼロ終了（未認証・
  存在しない item など）、タイムアウトは `isError: true` のテキストとして返し、エージェントが内容を読んで
  自己修復できるようにします。`op` の非ゼロ終了では、その stderr を（上限付きで）診断として添えます。

## 設計上の選択

- **読み取り専用に限定**: secret の解決と参照だけを公開し、書き込み・削除・任意コマンド実行
  （`op run` 等）は公開しません。エージェントに渡す権限を「読むだけ」に絞り、事故の影響を抑えます。
- **認証はサーバの外**: 資格情報やサインインはこのサーバが扱わず、`op`（環境）に委ねます。
  サーバ自身は状態を持たないため、対話サインイン・サービスアカウントのどちらでも同じく動きます。
- **依存追加なし**: 1Password の SDK ではなく `op` CLI へシェルアウトし、`serde_json` のみで同期的に
  JSON-RPC を処理します。引数組み立てと dispatch は純ロジックとしてユニットテストし、テスト不能な
  stdin ループ・シェルアウトだけをカバレッジ対象外にしています（[03-mcp.md](03-mcp.md) と同方針）。
- **protocolVersion**: `2024-11-05` を返します。
