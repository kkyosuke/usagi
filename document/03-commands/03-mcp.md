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
  作成し、特定のセッションのエージェントにプロンプトを送って作業を委譲し、各セッションの進捗（エージェントの
  phase・worktree の git 状態）をポーリングし、セッションに紐づく PR を取得し、不要になったセッションを削除
  できます。コーディネータ役のエージェントが、並行する worktree にタスクを振り分け、完了を検知して片付ける
  オーケストレータとして振る舞えます。

> MCP の tool 面は CLI と 1:1 対応ではなく、**エージェントが選びやすいワークフロー単位**に寄せています。
> 一覧と検索は `issue_search` / `memory_search` の 1 tool（`query` 省略で全件）に、メモリの保存と更新は
> `memory_save` の 1 tool（upsert）に、セッションへのプロンプト送信は配送先を `mode` で選ぶ `session_prompt`
> の 1 tool に統合。さらに「issue を新セッションに委譲」の定番手順を `session_delegate_issue` の 1 tool に
> まとめています。CLI は人間向けに `list` / `search` / `update` を別コマンドのまま残します（IF ごとに最適化）。

## 目次

- [概要](#概要)
- [起動と登録](#起動と登録)
- [アーキテクチャ](#アーキテクチャ)
- [対応 tool 一覧](#対応-tool-一覧)
- [`session_status` の挙動](#session_status-の挙動)
- [`session_prompt` の挙動](#session_prompt-の挙動)
- [`session_delegate_issue` の挙動](#session_delegate_issue-の挙動)
- [`session_remove` の挙動](#session_remove-の挙動)
- [ルートでの書き込みガードレール](#ルートでの書き込みガードレール)
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
- **ルートでの書き込みガードレール**: workspace root で起動したとき（issue / memory の対象と session の対象が
  一致するとき）は、git 追跡下の issue ストアを汚す書き込み系 issue tool（`issue_create` / `issue_update` /
  `issue_delete`）を**拒否**します。メモリストアは git 管理外のため memory 書き込みは拒否しません
  （[ルートでの書き込みガードレール](#ルートでの書き込みガードレール)）。
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
presentation/mcp/usagi.rs  … UsagiMcpServer：issue/memory サーバと session サーバを合成し tool をマージ。
        │                        さらに合成層だけの session_delegate_issue（両サーバの tool を順に呼ぶ）を追加
        ├─ presentation/mcp/issue/   … issue tool 実装。memory tool をマージして公開
        │   └ presentation/mcp/memory.rs … memory tool 実装（issue サーバが呼ぶ。save は upsert 1 本）
        └─ presentation/mcp/session.rs … session tool 実装（prompt の配送/remove は AgentBackend 経由）
        │  各 tool が呼ぶ
        ▼
usecase/issue, usecase/memory, usecase/session … create/get/search/update/delete・worktree 生成 ほか
        │
        ▼
infrastructure/{issue_store, memory_store} … <repo>/.usagi/{issues,memory}/ の markdown + index.json
（テスト時）FakeBackend / （本番）CliAgentBackend
  └─ session_prompt → mode(auto) が agent phase を見て振り分け
       ├─ queue → agent-prompts/ へキュー（TUI がフレッシュ起動時に消費／設定 ON なら稼働中に自動 spawn）
       └─ live  → agent-live-prompts/ へキュー（起動中 TUI が live pane へ注入）
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/mcp.rs` | `usagi mcp` のエントリ。カレントディレクトリ（issue / memory 用）とそこから解決した workspace root（`usecase/session::workspace_root`。session 用）を `UsagiMcpServer` に渡して構築し、stdin を 1 行ずつ読み `handle_line` の戻り値を stdout へ書く。本番 `AgentBackend`（`session_prompt` のプロンプトを配送先に応じて `agent-prompts/` か `agent-live-prompts/` へキューし、agent phase ファイルからライブペインの有無を判定し、`session_remove` で実効設定の agent を解決して `usecase/session::remove` を呼ぶ）もここに置く。ブロッキング I/O のみで、`hop` 同様カバレッジ計測の対象外。 |
| `presentation/mcp/mod.rs` | JSON-RPC 2.0 の共有フレーミング（`dispatch_line` / レスポンス整形 / `McpService` トレイト）。各サーバが共有。 |
| `presentation/mcp/usagi.rs` | `usagi` サーバの `UsagiMcpServer`。issue/memory サーバと session サーバを合成し、`tool_schemas` / `call_tool` で両者の tool をマージ・振り分けて 1 サーバで公開する。両サーバにまたがる `session_delegate_issue`（issue のプロンプト化→セッション作成→プロンプト投入を順に呼ぶ）はこの合成層が持つ。root での書き込みガードレール（[ルートでの書き込みガードレール](#ルートでの書き込みガードレール)）もこの層が振り分け前に判定する。ユニットテストで網羅。 |
| `presentation/mcp/issue/` | issue tool を提供する `McpServer`。`tool_schemas` / `call_tool` で `presentation/mcp/memory.rs` の memory tool をマージする。 |
| `presentation/mcp/memory.rs` | memory tool の実装（スキーマ・引数パース・`usecase/memory` への委譲）。issue サーバから呼ばれる。 |
| `presentation/mcp/session.rs` | session tool を提供する `SessionMcpServer`。実エージェント・実ファイルに触れる操作（`session_prompt` の 2 チャネル配送・ライブペイン検知・`session_remove`）を `AgentBackend` トレイトで抽象化し、ユニットテストで網羅。`mode` → チャネルの振り分けはサーバ側（テスト可能）で決める。 |
| `usecase/issue`・`usecase/memory`・`usecase/session` ほか | tool が呼ぶビジネスロジック。MCP 固有の知識は持たない。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase → infrastructure`。MCP 層は
presentation に閉じています（[2. アーキテクチャ](../02-architecture.md) 参照）。

## 対応 tool 一覧

`tools/list` で以下の 19 tool（issue 6 + memory 4 + session 8 + オーケストレーション 1）を公開します。結果はいずれも JSON テキストで
返ります。

| tool | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `issue_create` | `title` | `priority` / `labels` / `dependson` / `related` / `parent` / `milestone` / `body` | 作成された issue |
| `issue_get` | `number` | — | issue（存在しなければ `null`） |
| `issue_to_prompt` | `number` | — | `{ "number": N, "prompt": "…", "title": "…" }`（issue が無ければ実行エラー） |
| `issue_search` | — | `query` / `status` / `priority` / `label` / `parent` / `milestone` / `ready` | issue 配列（各要素に `ready` と `unmet_deps` を付与）。`query` 省略で全件、指定で全文検索 |
| `issue_update` | `number` | `title` / `status` / `priority` / `labels` / `dependson` / `related` / `parent` / `milestone` / `body` | 更新後の issue |
| `issue_delete` | `number` | — | `{ "number": N, "deleted": bool }` |
| `memory_save` | `name` | `title` / `type` / `related` / `body` | 保存されたメモリ（upsert。既存は部分更新、新規は `title` 必須） |
| `memory_get` | `name` | — | メモリ（存在しなければ `null`） |
| `memory_search` | — | `query` / `type` | メモリ配列（`updated_at` の新しい順）。`query` 省略で全件、指定で全文検索 |
| `memory_delete` | `name` | — | `{ "name": "…", "deleted": bool }` |
| `session_create` | `name` | — | 作成されたセッション（`name` / `root` / `worktrees`） |
| `session_list` | — | — | セッション配列（各要素に `name` / `display_name` / `root` / `created_at` / `worktrees`） |
| `session_status` | — | — | セッション配列（各要素に `name` / `display_name` / `root` / `agent_phase` / `worktrees`。各 worktree に `status` / `dirty` / `merged`）（[挙動](#session_status-の挙動)） |
| `session_prompt` | `name` / `prompt` | `mode`（`auto` / `queue` / `live`、既定 `auto`） | `{ "name": "…", "delivered_to": "queue" \| "live", "detail": "…" }`（[挙動](#session_prompt-の挙動)） |
| `session_pr` | `name` | — | `{ "name": "…", "root": "…", "merged": bool, "pr": [{ "number": N, "url": "…", "state": "open" \| "merged" }] }` |
| `session_remove` | `name` | `force` | `{ "name": "…", "removed": bool, "dirty": [worktree…] }`（[挙動](#session_remove-の挙動)） |
| `session_delegate_issue` | `number` | `name` | `{ "issue": N, "title": "…", "session": "…", "root": "…", "worktrees": […], "delivered_to": "queue" }`（[挙動](#session_delegate_issue-の挙動)） |

- `status` は `todo` / `in-progress` / `done`、`priority` は `high` / `medium` / `low`、`type`（memory）は `user` / `feedback` / `project` / `reference`。
- **`memory_save` は upsert 1 本**です。`name` が既存なら**渡したフィールドだけを部分更新**（未指定は保持、`created_at` も保持）、無ければ新規作成（このとき `title` 必須）。`name` は与えた文字列をスラッグ化して識別子にします。別途の `memory_update` tool はありません（body だけ直したいときは `name` と `body` だけ渡せば type 等は保たれます）。
- **一覧は検索の特殊形として統合**しています。`issue_search` / `memory_search` は `query` を省略すると全件を返し（空クエリはすべてに一致）、`query` を与えると全文検索に絞り込みます。フィルタ（`status` / `type` など）は `query` の有無にかかわらず併用できます。別途の `issue_list` / `memory_list` tool はありません。
- `dependson` はブロックする先行条件、`related` はブロックしない関連、`parent` は所属（Epic ⊃ サブタスク）、`milestone` は束ね。`issue_search` は `parent` / `milestone` でも絞り込めます。
- `issue_update` の `parent` / `milestone` は三状態です: 省略すると変更なし、**`null` を明示すると解除**、値を渡すと設定します。
- `issue_search` は CLI と同じく **`dependson` がすべて `done` の issue を `ready: true`**
  とし、未達の依存番号を `unmet_deps` に入れて返します（着手可能なタスクの判別用）。
- `ready: true`（引数）を渡すと着手可能な issue だけに絞り込みます。
- `issue_to_prompt` は issue を **そのまま実行できるエージェント向けプロンプト**に整形して返します
  （実装手順・status 更新の指示と issue 本文を含む）。プロンプトはリポジトリ非依存の文言で、特定言語の
  コマンドや usagi 固有のパスは埋め込みません（リポジトリ側の規約ドキュメントに従わせます）。
  `issue_to_prompt(number)` → `session_create(name)` → `session_prompt(name, prompt)` の 3 手を
  1 回で行うのが **`session_delegate_issue`** です（[挙動](#session_delegate_issue-の挙動)）。プロンプトを
  自分で調整したい／既存セッションに載せたい場合はこの primitive 3 つを直接使います。
- `session_delegate_issue` は「issue を新しいセッションに委譲して着手させる」というオーケストレーションの
  定番手順を 1 tool にまとめたものです。issue をプロンプト化し、`name`（既定 `issue-<番号>`）でセッションを
  作成し、そのプロンプトを起動時キューに積むまでを行います（[挙動](#session_delegate_issue-の挙動)）。
- `session_create` は `name` をセッション名として `<root>/.usagi/sessions/<name>/` に worktree を生成します
  （各リポジトリで切るブランチは `usagi/<name>`）。空・パス区切り文字を含む名前は拒否し、
  既存のセッション名は重複エラーになります（CLI と同じ検証）。`session_list` は `state.json` を読むだけの
  軽量クエリで、on-disk の reconcile は行いません。
- `session_create` は worktree を生成するだけで、動作中の TUI の[在席](../design/home/02-layout.md#在席focus)には
  入りません（`usagi mcp` は TUI を操作できない別プロセスのため）。TUI から作成したときは作成完了後にその
  セッションへ自動で在席しますが、MCP 経由の作成はホーム画面の一覧にバックグラウンドで反映されるだけで
  カーソルは動きません。
- `session_pr` は、対象セッションのエージェント出力から検出され、TUI の PR バッジとして表示される
  PR URL を返します。各 PR には `state`（セッションの全 worktree がデフォルトブランチにマージ済みなら
  `merged`、それ以外は `open`）が付き、返り値トップレベルの `merged` も同じ判定を返します。この状態は
  キャッシュ済みの worktree 状態から導出し（usagi は GitHub に問い合わせません）、**マージせずにクローズ
  された PR は open と区別できません**。PR が記録されていないセッションは `pr: []` を返します。存在しない
  セッション名は実行エラー（`isError: true`）になります。
- `session_status` は、コーディネータ役のエージェントがポーリングして委譲先の進捗を知るための読み取り専用
  tool です。エージェントの phase と各 worktree の git 状態をキャッシュから返します（git を起動しません）
  （[挙動](#session_status-の挙動)）。
- `session_prompt` は 1 つの tool で 2 つの配送チャネル（起動時キュー / 起動中ペイン）を持ち、`mode` で
  選びます。既定の `auto` はライブペインの有無を検知して自動で振り分けます。どちらに配送したかは返り値の
  `delivered_to` でわかります（[挙動](#session_prompt-の挙動)）。`name` に予約名 **`:root`** を渡すと、セッション
  ではなく**ルート行のコーディネータ**へ配送でき、子セッションが完了を push で報告できます
  （[ルート行への push 型完了報告](#ルート行コーディネータへの-push-型完了報告)）。
- `session_remove` はセッションの全 worktree とブランチを破棄し、コピーされたファイルとエージェントの会話履歴を
  消して `state.json` から削除します。`usagi clean` が起動するバックグラウンドエージェントは、このツールで
  放置セッションを片付けます（[挙動](#session_remove-の挙動)）。
- `session_list` / `session_create` の各 worktree 要素は `path` / `branch` / `head` / `primary` / `status` を持ちます
  （保存フォーマットの正本は [data/02-workspace.md](../data/02-workspace.md#statejson)）。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## `session_status` の挙動

`session_status` は、コーディネータ役のエージェントが**委譲先セッションの進捗をポーリング**して、子の完了や
PR のマージを検知し、`session_remove` → 次の issue 委譲へと自律ループを回すための読み取り専用 tool です。
各セッションについて次を返します。

| フィールド | 内容 |
|---|---|
| `agent_phase` | セッションの root worktree に記録されたエージェントの lifecycle phase（`ready` / `running` / `waiting` / `ended`）。ペインが一度も起動していない（またはペインが死んで phase がクリアされた）場合は `none`。TUI のバッジを駆動するのと同じ agent phase ファイルを読む。 |
| `worktrees[].status` | worktree の git 状態（`new` / `dirty` / `local` / `pushed` / `synced`）。`usagi status`（[`state.json`](../data/02-workspace.md#statejson)）と同じ分類。 |
| `worktrees[].dirty` | 未コミットの変更がある（`status == dirty`）。 |
| `worktrees[].merged` | デフォルトブランチがこの worktree の内容をすべて含む＝マージ済み（`status == synced`）。 |

- **読み取り専用・軽量**です。`state.json`（直近の同期で記録した worktree 状態）と agent phase ファイルだけを
  読み、**git を起動しません**。値の鮮度は直近の[ワークスペース同期](../data/02-workspace.md#statejson)
  （稼働中の TUI がバックグラウンドで実行）に一致します。ポーリング用途で繰り返し呼んでも安価です。
- `agent_phase` の `ended` は「子エージェントがターンを終えた／プロセスが終了した」ことを、`merged` は
  「作業がデフォルトブランチに取り込まれた」ことを示します。コーディネータはこの 2 つを見てセッションの完了を
  判定し、`session_remove` で片付けてから次の issue を委譲します。
- `merged` は worktree 状態から導出するため、リモートでマージされた反映にはローカルの
  `origin/<default>` が最新である必要があります（`usagi status` と同じ制約。GitHub には問い合わせません）。
- セッションが 1 件も無ければ空配列 `[]` を返します。並び順は `session_list` と同じ（ホーム一覧の表示順）。

## `session_prompt` の挙動

`session_prompt` は対象セッションのエージェントにプロンプトを渡す唯一の tool です。その場ではエージェントの
応答を返さず（起動もしません）、**2 つの配送チャネル**のいずれかにプロンプトを託します。`usagi mcp` は
動作中の TUI に手を伸ばしてペインを操作できない別プロセスのため、どちらもファイル経由でキューします。

| チャネル | キュー先 | 配送タイミング |
|---|---|---|
| 起動時キュー（queue） | [`agent-prompts/`](../data/01-global.md#agent-prompts) | ホーム画面がそのセッションのエージェントペインを**次にフレッシュ起動するとき**に、エージェントの**最初のメッセージ**として渡す。設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が有効なら、TUI 稼働中はホーム画面がキューを検知してペインを**バックグラウンドで自動 spawn** し、人が開くのを待たない（[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)） |
| ライブキュー（live） | [`agent-live-prompts/`](../data/01-global.md#agent-live-prompts) | **すでに起動中のエージェントペイン**へ、動作中の TUI の監視スレッドが「貼り付け → Enter」で流し込む |

どちらを使うかは `mode` 引数で決めます（省略時 `auto`）。

- **`auto`（既定）**: セッションにライブなエージェントペインが検知できれば **live**、なければ **queue** を選びます。
  ペインの有無は、エージェントの lifecycle フックが worktree 別に記録する agent phase ファイル（ペインが死ぬと
  ホーム画面がクリアする）で判定します。**呼び出し側はエージェントが起動中かどうかを知らなくてよい**のが利点です。
- **`queue`**: ライブペインの有無にかかわらず、常に起動時キューへ入れます。
- **`live`**: 常にライブキューへ入れます（ペインがまだ無ければ、開くまでキューで待ちます）。

返り値の `delivered_to` に、実際に配送したチャネル（`"queue"` / `"live"`）が入るため、`auto` を使っても
どちらに届いたかが確認できます。`detail` にはチャネルごとの確認メッセージが入ります。

### ルート行（コーディネータ）への push 型完了報告

`name` に予約名 **`:root`** を渡すと、セッションではなく**ワークスペースのルート行**（[`⌂ root`](../04-orchestration.md#用語)）を
配送先にできます。ルート行で動くコーディネータ役のエージェントへ、子セッションのエージェントが**自分の完了を
push で報告**するための経路です。ポーリング（[`session_status`](#session_status-の挙動)）と違い、ポーリング間隔を
待たずに完了が即座にコーディネータの入力として届くため、コーディネータは次のタスクへすぐ進めます。

- `:root` はルート行の作業ディレクトリ（= workspace root）へ配送します。ルート行はどのセッションにも属さないため、
  `:root` の解決に**セッションの存在は不要**です。配送チャネルの選び方（`mode` / `auto` の live・queue 判定）は
  通常のセッション宛と同じで、コーディネータのペインが起動中なら **live** で即座に流し込み、起動していなければ
  ルート行の agent ペインを**次にフレッシュ起動したとき**の起動時キューへ積みます。
- 予約名は先頭に `:` を付けた `:root` です。セッション名は git ブランチ / ディレクトリ名になるため先頭 `:` を
  持たず、`:root` が実在のセッションと衝突・混同することはありません。
- 使い方は、委譲された子セッションのエージェントが作業を終えたときに、`session_prompt(name=":root", prompt="…完了報告…")`
  を自分で呼ぶだけです（`mode` は既定の `auto` でよい）。報告文面は子エージェントが決められるため、完了した issue 番号・
  開いた PR 番号・残課題などをコーディネータにそのまま伝えられます。

- **起動時キュー**にキューしたプロンプトは、[在席](../design/home/02-layout.md#在席focus)から `agent` を実行して
  **エージェントペインを新規 spawn する**ときに 1 回だけ消費されます（再アタッチや 2 枚目のエージェントタブには
  再送されません）。フレッシュ起動が起きるまではキューに残ります。引き渡し方はエージェント CLI 依存で、Claude は
  起動時の位置引数（`claude … '<prompt>'`）として受け取り、対話モードのままそのプロンプトに着手します。Gemini は
  この経路を持たないため素起動します。設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が
  有効なら、この「フレッシュ起動」を人が起こすのを待たず、TUI がキューを検知してバックグラウンドで自動的に行います
  （[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)）。OFF にすると従来どおり人がペインを開くまで消費されません。
- **ライブキュー**のプロンプトは、対象セッションに live agent ペインがある場合 TUI の監視 tick（約 200 ms 間隔）で
  配送されます。複数回送ったプロンプトは追記順に、各 1 回だけ配送されます。配送は「取り出し → 書き込み」の順で
  行い、PTY への書き込みが失敗したプロンプト（および同じ tick でそれ以降にあった未配送分）はライブキューの
  **先頭へ戻して**次の tick で再試行するため、キュー済みと返答したのに黙って失われることはありません。書き込みは
  端末の paste と同じ扱いで、対象プログラムが bracketed paste mode を有効にしているときはプロンプト全体を
  bracketed paste で包んでから Enter を送り、複数行プロンプトが途中で複数回 submit されるのを避けます。
- 作業はどちらのチャネルでもセッションのブランチ（worktree）上で隔離されます。
- `prompt` にはサイズ上限（128 KiB）があります。超えるとチャネルへ書き込む前にツールエラーとして拒否します。

## `session_delegate_issue` の挙動

`session_delegate_issue` は、コーディネータ役のエージェントが最も多用する「issue を新しいセッションに委譲して
着手させる」手順を 1 呼び出しにまとめた**オーケストレーション tool**です。次の 3 ステップを順に行います。

1. `issue_to_prompt(number)` で issue を実行プロンプトに整形する（issue が無ければ実行エラー）。
2. `session_create(name)` でセッションを作成する（`name` 既定は `issue-<番号>`。名前が既存なら重複エラー）。
3. `session_prompt(name, prompt, mode=queue)` でそのプロンプトを起動時キューに積む。

- 新規作成したセッションには live なエージェントペインが存在しないため、配送は常に**起動時キュー**（`queue`）です。
  返り値の `delivered_to` も常に `"queue"` になります。
- 設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が有効で TUI が稼働していれば、委譲した
  セッションの agent ペインは人が開かなくても**バックグラウンドで自動起動**され、キュー済みプロンプトに着手します
  （[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)）。OFF なら次に人がそのペインをフレッシュ起動するまでキューで待ちます。
- **新しいロジックは足していません**。既存の 3 tool（`issue_to_prompt` / `session_create` / `session_prompt`）を
  合成サーバ（`usagi.rs`）が順に呼ぶだけなので、採番・検証・キューなどの挙動は primitive と完全に一致します。
- primitive はそのまま残っています。**プロンプトを手で調整したい**、**既存セッションに載せたい**、**live 送信したい**
  といったケースでは `issue_to_prompt` → `session_prompt`（`mode` 指定）を直接使ってください。
- 途中のステップが失敗すると（issue 不在・セッション名重複など）その時点でツールエラーを返します。

## `session_remove` の挙動

`session_remove` はセッションを物理的に破棄します。CLI / TUI のセッション削除（[`session remove`](02-tui.md#session)）と
同じ `usecase/session::remove` を呼ぶため、挙動は一致します。

- 全リポジトリの worktree とセッションブランチを取り外し、コピーされたファイルを削除し、各 worktree の
  エージェント会話履歴（例: Claude のトランスクリプト）と usagi が記録する agent phase を消してから、`state.json`
  の記録を落とします。会話履歴を消す対象 CLI は、ワークスペースの実効設定（`agent_cli`）から解決します。
- **未コミットの変更がある worktree は、既定では削除しません**。この場合 `removed: false` を返し、ブロック要因の
  worktree を `dirty` 配列で示します。`force: true`（任意引数、既定 `false`）を渡すとその変更を破棄して削除します。
- 存在しないセッション名は実行エラー（`isError: true`）になります。

## ルートでの書き込みガードレール

コーディネータは workspace root（`main` のチェックアウト）で `usagi mcp` を動かして、並行するセッションへ issue を
委譲したり進捗をポーリングしたりします。このとき **root は git 追跡下のリポジトリを変更しない**（[4. オーケストレーション](../04-orchestration.md) の原則）
ことを、規約ではなく**技術的に**担保するためのガードレールです。

`usagi mcp` は issue / memory をカレントの worktree に、session を workspace root に解決します（[概要](#概要)）。
root で起動するとこの 2 つの対象が一致するため、その一致を「root で動いている」の判定に使い、**git 追跡下の issue
ストア（`.usagi/issues/`）を汚す書き込み系 issue tool を拒否**します。**メモリストア（`.usagi/memory/`）は git 管理外**
（`.usagi/.gitignore` で除外）のため、root で書き込んでも追跡ツリーは汚れず、`memory_save` / `memory_delete` は拒否
しません。

| tool | root（対象が一致） | セッション worktree |
|---|---|---|
| `issue_create` / `issue_update` / `issue_delete` | 拒否（`isError: true`） | 実行可 |
| `memory_save` / `memory_delete` | 実行可（git 管理外） | 実行可 |
| `issue_get` / `issue_search` / `issue_to_prompt` | 実行可 | 実行可 |
| `memory_get` / `memory_search` | 実行可 | 実行可 |
| すべての `session_*` / `session_delegate_issue` | 実行可 | 実行可 |

- 拒否は tool 実行エラー（`isError: true`）として返し、「root では実行できない・セッション worktree 内で行うこと」を
  案内します。エージェントはこのテキストを読んで、`session_create` / `session_delegate_issue` でセッションを開いてから
  書き込むよう自己修復できます。
- 読み取り・整形（`issue_get` / `issue_search` / `issue_to_prompt` / `memory_get` / `memory_search`）と、
  オーケストレーションに必要な `session_*`・`session_delegate_issue` は root でも許可します。既存の issue を
  プロンプト化してセッションに委譲する、というコーディネータの主要な動線は root のまま回せます。
- パスの一致は**正規化して比較**します（`canonicalize` でシンボリックリンクや `/tmp` ⇄ `/private/tmp` の差を吸収し、
  正規化できないときは素の比較にフォールバック）。カレントが非正規パスでも root を取りこぼしません。
- 判定は合成層（`usagi.rs`）に閉じており、issue / memory / session の各サブサーバは無改変です。セッション worktree
  （対象が一致しない）では全 tool が従来どおり動作します（回帰なし）。

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
  - 例: 不正な引数（必須項目の欠落・型不一致）、未知の tool 名、`issue_update` の対象が存在しない、
    root での書き込み系 issue tool の拒否（[ルートでの書き込みガードレール](#ルートでの書き込みガードレール)）。

## 設計上の選択

- **自前実装（依存追加なし）**: MCP の SDK（`rmcp` 等）は tokio など非同期スタックを要しますが、本
  サーバは `serde_json` のみで同期的に実装しています。usagi の「依存を最小に保つ」「テストカバレッジ
  100%」という方針に合わせ、protocol 分岐を純粋関数（`handle_line`）に閉じ込めてユニットテストで
  網羅し、テスト不能な stdin ループだけをカバレッジ対象外にしています。
- **protocolVersion**: `2024-11-05` を返します。
- **状態を持たない**: サーバは内部状態を保持せず、各 tool 呼び出しが `.usagi/issues/` / `.usagi/memory/` /
  `state.json` を直接読み書きします。CLI・TUI と MCP を混在して使っても整合します。
- **1 サーバに合成**: issue/memory（リポジトリの純粋な読み書き）と session（`session_prompt` で
  プロンプトをキューする `AgentBackend` を要する）は依存関係が異なるため、それぞれ独立にユニットテストされた別サーバの
  まま `usagi.rs` で合成し、tool のマージと振り分けだけをこの層が担います。これにより登録は `usagi` 1 つで
  済みつつ、各サーバの責務とテストは分離されます。
- **tool はワークフロー単位に統合**: CRUD をそのまま tool 化せず、エージェントの意図に寄せています。
  - **重複の統合**: 一覧/検索は `*_search`（`query` 省略で全件）に、メモリの保存/更新は `memory_save`（upsert）に、
    起動時キュー/ライブ送信は `session_prompt`（`mode`）に畳み込み、選ぶ tool 数と紛らわしい 2 択を減らす。
  - **手順の統合**: 頻出のオーケストレーション（issue→新セッション委譲）を `session_delegate_issue` の 1 呼び出しに。
    ただし primitive（`issue_to_prompt` / `session_create` / `session_prompt`）は残し、細かい制御が要るときはそれらを使う。
  - issue/memory の CRUD は「エージェントが所有するデータストアの素の操作」で、無理に融合すると機能が隠れるため
    残しています。CLI（人間向け）とは IF を分けて最適化しています。
