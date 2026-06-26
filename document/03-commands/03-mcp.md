# 3.3 MCP サーバ（`usagi mcp`）

> [コマンドリファレンス](README.md) ｜ ← 前へ [3.2 TUI 内コマンド](02-tui.md) ｜ 次へ → [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md)

`usagi mcp` は、usagi の操作を **MCP（Model Context Protocol）サーバ**として AI エージェントに公開する
コマンドです。1 つの `usagi mcp` プロセスが次の 3 系統の tool をまとめて提供し、エージェント
（Claude Code など）は単一の `usagi` サーバを登録するだけで済みます。

- **issue**: `usagi issue`（[01-cli.md](01-cli.md#usagi-issue)）と同じタスク issue 操作（起票・参照・更新）に加え、
  issue をエージェント向け実行プロンプトに整形する `issue_to_prompt`。
- **memory**: `usagi memory`（[01-cli.md](01-cli.md#usagi-memory)）と同じメモリ操作（セッションをまたいで
  覚えておく知識の保存・想起）。
- **session**: usagi のセッション（[4. オーケストレーション](../04-orchestration.md)）操作。セッションを
  作成し、特定のセッションのエージェントにプロンプトを送って作業を委譲し、不要になったセッションを削除できます。
  コーディネータ役のエージェントが、並行する worktree にタスクを振り分けるオーケストレータとして振る舞えます。

## 目次

- [概要](#概要)
- [起動と登録](#起動と登録)
- [アーキテクチャ](#アーキテクチャ)
- [対応 tool 一覧](#対応-tool-一覧)
- [`session_prompt` の挙動](#session_prompt-の挙動)
- [`session_remove` の挙動](#session_remove-の挙動)
- [JSON-RPC プロトコル](#json-rpc-プロトコル)
- [エラーハンドリング](#エラーハンドリング)
- [設計上の選択](#設計上の選択)

## 概要

- **トランスポート**: stdio（標準入出力）上の **JSON-RPC 2.0**。1 メッセージ = 1 行の JSON。
- **対象リポジトリ / ワークスペース**: `usagi mcp` を起動したカレントディレクトリから解決します。
  **issue / memory はカレントの worktree**（`.usagi/issues/` と `.usagi/memory/`）を対象にし、
  **session は workspace root**（`.usagi/sessions/` と `state.json`）を対象にします。カレントディレクトリが
  セッションツリー（`<workspace>/.usagi/sessions/<name>/`）の中にある場合、issue / memory はそのセッション
  自身の `.usagi/` に書き（ブランチに乗って PR で `main` へ流れる）、session は workspace root に解決します
  （[起動と登録](#起動と登録)）。
- **ロジックの共有**: 各 tool は CLI・TUI と同じ [`usecase/issue`](../02-architecture.md#各層の責務) /
  `usecase/memory` / `usecase/session` を呼ぶ薄いアダプタ。挙動（採番・依存 readiness 判定・メモリの
  upsert・worktree 生成・`state.json` 記録など）は完全に一致します。

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

issue・memory・session の tool はすべてこの 1 サーバが提供するため、登録は `usagi` の 1 エントリだけで
済みます（usagi がエージェント起動時に自動で wire する `--mcp-config` も同じく `usagi` 1 エントリです）。

エージェントがセッションツリー（`<workspace>/.usagi/sessions/<name>/`）の中で起動された場合、
**issue / memory はそのセッション自身の `.usagi/` に保存**されます。セッションのブランチに乗り、PR 経由で
`main` に流れるため、ワークスペースのチェックアウトを未コミットで汚しません。issue の採番だけは worktree を
またいで一意にするためワークスペース全体を横断します（[採番](../data/03-issues.md#採番ワークスペース横断)）。
一方 **session 操作は常に workspace root** に解決します（並行 worktree 全体を管理するため）。

> セッションの worktree に保存した issue / memory は、その**ブランチをマージしないまま破棄すると失われます**
> （`usagi clean` / `session remove`）。「作業（ブランチ）と一緒に issue 変更も流れる」セマンティクスを優先した
> トレードオフです。

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
presentation/cli/mcp.rs    … stdin ループ + エージェント CLI バックエンド（薄い I/O ラッパ。カバレッジ対象外）
        │  handle_line(line) ごとに委譲
        ▼
presentation/mcp/usagi.rs  … UsagiMcpServer：issue/memory サーバと session サーバを合成し tool をマージ
        ├─ presentation/mcp/issue/   … issue tool 実装。memory tool をマージして公開
        │   └ presentation/mcp/memory.rs … memory tool 実装（issue サーバが呼ぶ）
        └─ presentation/mcp/session.rs … session tool 実装（prompt は AgentBackend 経由）
        │  各 tool が呼ぶ
        ▼
usecase/issue, usecase/memory, usecase/session … create/get/list/search/update/delete・worktree 生成 ほか
        │
        ▼
infrastructure/{issue_store, memory_store} … <repo>/.usagi/{issues,memory}/ の markdown + index.json
（テスト時）FakeBackend / （本番）CliAgentBackend → agent-prompts/ へキュー（TUI が起動時に消費）
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/mcp.rs` | `usagi mcp` のエントリ。カレントディレクトリ（issue / memory 用）とそこから解決した workspace root（`usecase/session::workspace_root`。session 用）を `UsagiMcpServer` に渡して構築し、stdin を 1 行ずつ読み `handle_line` の戻り値を stdout へ書く。本番 `AgentBackend`（`session_prompt` のプロンプトを `agent-prompts/` へキューし、`session_remove` で実効設定の agent を解決して `usecase/session::remove` を呼ぶ）もここに置く。ブロッキング I/O のみで、`hop` 同様カバレッジ計測の対象外。 |
| `presentation/mcp/mod.rs` | JSON-RPC 2.0 の共有フレーミング（`dispatch_line` / レスポンス整形 / `McpService` トレイト）。各サーバが共有。 |
| `presentation/mcp/usagi.rs` | `usagi` サーバの `UsagiMcpServer`。issue/memory サーバと session サーバを合成し、`tool_schemas` / `call_tool` で両者の tool をマージ・振り分けて 1 サーバで公開する。ユニットテストで網羅。 |
| `presentation/mcp/issue/` | issue tool を提供する `McpServer`。`tool_schemas` / `call_tool` で `presentation/mcp/memory.rs` の memory tool をマージする。 |
| `presentation/mcp/memory.rs` | memory tool の実装（スキーマ・引数パース・`usecase/memory` への委譲）。issue サーバから呼ばれる。 |
| `presentation/mcp/session.rs` | session tool を提供する `SessionMcpServer`。実エージェント・実ファイルに触れる操作（`session_prompt` / `session_remove`）を `AgentBackend` トレイトで抽象化し、ユニットテストで網羅。 |
| `usecase/issue`・`usecase/memory`・`usecase/session` ほか | tool が呼ぶビジネスロジック。MCP 固有の知識は持たない。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase → infrastructure`。MCP 層は
presentation に閉じています（[2. アーキテクチャ](../02-architecture.md) 参照）。

## 対応 tool 一覧

`tools/list` で以下の 17 tool（issue 7 + memory 6 + session 4）を公開します。結果はいずれも JSON テキストで
返ります。

| tool | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `issue_create` | `title` | `priority` / `labels` / `dependson` / `related` / `parent` / `milestone` / `body` | 作成された issue |
| `issue_get` | `number` | — | issue（存在しなければ `null`） |
| `issue_to_prompt` | `number` | — | `{ "number": N, "prompt": "…", "title": "…" }`（issue が無ければ実行エラー） |
| `issue_list` | — | `status` / `priority` / `label` / `parent` / `milestone` / `ready` | issue 配列（各要素に `ready` と `unmet_deps` を付与） |
| `issue_search` | `query` | `status` / `priority` / `label` / `parent` / `milestone` / `ready` | 一致した issue 配列（`list` と同形式） |
| `issue_update` | `number` | `title` / `status` / `priority` / `labels` / `dependson` / `related` / `parent` / `milestone` / `body` | 更新後の issue |
| `issue_delete` | `number` | — | `{ "number": N, "deleted": bool }` |
| `memory_save` | `name` / `title` | `type` / `related` / `body` | 保存されたメモリ（同名なら upsert） |
| `memory_get` | `name` | — | メモリ（存在しなければ `null`） |
| `memory_list` | — | `type` | メモリ配列（`updated_at` の新しい順） |
| `memory_search` | `query` | `type` | 一致したメモリ配列（`list` と同形式） |
| `memory_update` | `name` | `title` / `type` / `related` / `body` | 更新後のメモリ |
| `memory_delete` | `name` | — | `{ "name": "…", "deleted": bool }` |
| `session_create` | `name` | — | 作成されたセッション（`name` / `root` / `worktrees`） |
| `session_list` | — | — | セッション配列（各要素に `name` / `display_name` / `root` / `created_at` / `worktrees`） |
| `session_prompt` | `name` / `prompt` | — | プロンプトを対象セッションにキューした旨の確認メッセージ（[挙動](#session_prompt-の挙動)） |
| `session_remove` | `name` | `force` | `{ "name": "…", "removed": bool, "dirty": [worktree…] }`（[挙動](#session_remove-の挙動)） |

- `status` は `todo` / `in-progress` / `done`、`priority` は `high` / `medium` / `low`、`type`（memory）は `user` / `feedback` / `project` / `reference`。
- `memory_save` は **`name` が既存なら上書き**（in-place 更新、`created_at` は保持）。`name` は与えた文字列をスラッグ化して識別子にします。
- `dependson` はブロックする先行条件、`related` はブロックしない関連、`parent` は所属（Epic ⊃ サブタスク）、`milestone` は束ね。`issue_list` / `issue_search` は `parent` / `milestone` でも絞り込めます。
- `issue_update` の `parent` / `milestone` は三状態です: 省略すると変更なし、**`null` を明示すると解除**、値を渡すと設定します。
- `issue_list` / `issue_search` は CLI と同じく **`dependson` がすべて `done` の issue を `ready: true`**
  とし、未達の依存番号を `unmet_deps` に入れて返します（着手可能なタスクの判別用）。
- `ready: true`（引数）を渡すと着手可能な issue だけに絞り込みます。
- `issue_to_prompt` は issue を **そのまま実行できるエージェント向けプロンプト**に整形して返します
  （実装手順・status 更新の指示と issue 本文を含む）。プロンプトはリポジトリ非依存の文言で、特定言語の
  コマンドや usagi 固有のパスは埋め込みません（リポジトリ側の規約ドキュメントに従わせます）。
  `issue_to_prompt(number)` → `session_create(name)` →
  `session_prompt(name, prompt)` と組み合わせると、コーディネータ役のエージェントが「issue を特定の
  セッションのエージェントに実装させる」オーケストレーションを最小手数で組めます。
- `session_create` は `name` をセッション名（=全リポジトリで作成する新規ブランチ名）として
  `<root>/.usagi/sessions/<name>/` に worktree を生成します。空・パス区切り文字を含む名前は拒否し、
  既存のセッション名は重複エラーになります（CLI と同じ検証）。`session_list` は `state.json` を読むだけの
  軽量クエリで、on-disk の reconcile は行いません。
- `session_create` は worktree を生成するだけで、動作中の TUI の[在席](../design/05-home.md#在席focus)には
  入りません（`usagi mcp` は TUI を操作できない別プロセスのため）。TUI から作成したときは作成完了後にその
  セッションへ自動で在席しますが、MCP 経由の作成はホーム画面の一覧にバックグラウンドで反映されるだけで
  カーソルは動きません。
- `session_remove` はセッションの全 worktree とブランチを破棄し、コピーされたファイルとエージェントの会話履歴を
  消して `state.json` から削除します。`usagi clean` が起動するバックグラウンドエージェントは、このツールで
  放置セッションを片付けます（[挙動](#session_remove-の挙動)）。
- `session_list` / `session_create` の各 worktree 要素は `path` / `branch` / `head` / `primary` / `status` を持ちます
  （保存フォーマットの正本は [data/02-workspace.md](../data/02-workspace.md#statejson)）。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## `session_prompt` の挙動

`session_prompt` は、対象セッションの worktree に**プロンプトをキュー（queue）するだけ**で、その場では
エージェントを起動しません。`usagi mcp` は動作中の TUI に手を伸ばしてペインを操作できない別プロセスのため、
プロンプトを worktree 別の一時ファイル（[`agent-prompts/`](../data/01-global.md#agent-prompts)）へ保存し、
**ホーム画面がそのセッションのエージェントペインを次にフレッシュ起動するとき**に取り出して、エージェントの
**最初のメッセージ**として渡します。これにより委譲したプロンプトは、デタッチで走るのではなく、
セッションの**右ペイン**で対話的に実行されます。

- キューしたプロンプトは、[在席](../design/05-home.md#在席focus)から `agent` を実行して**エージェントペインを
  新規 spawn する**ときに 1 回だけ消費されます（再アタッチや、後から在席のアクションで `a`（agent）で開く 2 枚目のエージェント
  タブには再送されません）。フレッシュ起動が起きるまではキューに残ります。
- プロンプトの引き渡し方はエージェント CLI 依存です。Claude は起動時の位置引数（`claude … '<prompt>'`）として
  受け取り、対話モードのままそのプロンプトに着手します。Gemini はこの経路を持たないため素起動します。
- 作業はセッションのブランチ（worktree）上で隔離されます。同じ worktree を共有するため、キューしたプロンプトは
  その worktree のエージェントに届きます。

## `session_remove` の挙動

`session_remove` はセッションを物理的に破棄します。CLI / TUI のセッション削除（[`session remove`](02-tui.md#session)）と
同じ `usecase/session::remove` を呼ぶため、挙動は一致します。

- 全リポジトリの worktree とセッションブランチを取り外し、コピーされたファイルを削除し、各 worktree の
  エージェント会話履歴（例: Claude のトランスクリプト）と usagi が記録する agent phase を消してから、`state.json`
  の記録を落とします。会話履歴を消す対象 CLI は、ワークスペースの実効設定（`agent_cli`）から解決します。
- **未コミットの変更がある worktree は、既定では削除しません**。この場合 `removed: false` を返し、ブロック要因の
  worktree を `dirty` 配列で示します。`force: true`（任意引数、既定 `false`）を渡すとその変更を破棄して削除します。
- 存在しないセッション名は実行エラー（`isError: true`）になります。

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
- **状態を持たない**: サーバは内部状態を保持せず、各 tool 呼び出しが `.usagi/issues/` / `.usagi/memory/` /
  `state.json` を直接読み書きします。CLI・TUI と MCP を混在して使っても整合します。
- **1 サーバに合成**: issue/memory（リポジトリの純粋な読み書き）と session（`session_prompt` でプロンプトを
  キューする `AgentBackend` を要する）は依存関係が異なるため、それぞれ独立にユニットテストされた別サーバの
  まま `usagi.rs` で合成し、tool のマージと振り分けだけをこの層が担います。これにより登録は `usagi` 1 つで
  済みつつ、各サーバの責務とテストは分離されます。
