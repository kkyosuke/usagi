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
> まとめ、事前 issue を要さない**起源フロー**を `session_delegate_brief` の 1 tool にまとめています。CLI は
> 人間向けに `list` / `search` / `update` を別コマンドのまま残します（IF ごとに最適化）。

## 目次

- [概要](#概要)
- [起動と登録](#起動と登録)
- [アーキテクチャ](#アーキテクチャ)
- [対応 tool 一覧](#対応-tool-一覧)
- [`session_status` の挙動](#session_status-の挙動)
- [`session_prompt` の挙動](#session_prompt-の挙動)
- [`session_complete` の挙動](#session_complete-の挙動)
- [`session_delegate_issue` の挙動](#session_delegate_issue-の挙動)
- [`session_delegate_brief` の挙動](#session_delegate_brief-の挙動)
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
        │                        さらに合成層だけの session_delegate_issue / session_delegate_brief（既存 tool を順に呼ぶ）を追加
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
  └─ session_prompt → mode(auto) が live-pane マーカーを見て振り分け
       ├─ queue → agent-prompts/ へキュー（TUI がフレッシュ起動時に消費／設定 ON なら稼働中に自動 spawn）
       └─ live  → agent-live-prompts/ へキュー（起動中 TUI が live pane へ注入）
```

| モジュール | 役割 |
|---|---|
| `main.rs` / `presentation/cli/mcp.rs` | `usagi mcp` の合成ルートと stdio エントリ。カレントディレクトリ（issue / memory 用）とそこから解決した workspace root（`usecase/session::workspace_root`。session 用）を `UsagiMcpServer` に渡し、stdin の JSON-RPC を処理する。本番 `AgentBackend` はプロンプトキュー・[live-pane マーカー](../data/01-global.md#agent-live-panes)・session 削除を担う。モデル検証は `main.rs` が `usecase/agent` の本番 `CliAgentModelProbe` を構築して注入するだけに留め、transport は両 port の fake でユニットテストする。 |
| `presentation/mcp/mod.rs` | JSON-RPC 2.0 の共有フレーミング（`dispatch_line` / レスポンス整形 / `McpService` トレイト）。各サーバが共有。 |
| `presentation/mcp/usagi.rs` | `usagi` サーバの `UsagiMcpServer`。issue/memory サーバと session サーバを合成し、`tool_schemas` / `call_tool` で両者の tool をマージ・振り分けて 1 サーバで公開する。両サーバにまたがる `session_delegate_issue`（issue のプロンプト化→セッション作成→プロンプト投入）と `session_delegate_brief`（セッション作成→ブリーフのキュー投入）はこの合成層が持つ。root での書き込みガードレール（[ルートでの書き込みガードレール](#ルートでの書き込みガードレール)）もこの層が振り分け前に判定する。ユニットテストで網羅。 |
| `presentation/mcp/issue/` | issue tool を提供する `McpServer`。`tool_schemas` / `call_tool` で `presentation/mcp/memory.rs` の memory tool をマージする。 |
| `presentation/mcp/memory.rs` | memory tool の実装（スキーマ・引数パース・`usecase/memory` への委譲）。issue サーバから呼ばれる。 |
| `presentation/mcp/session.rs` | session tool を提供する `SessionMcpServer`。実エージェント・実ファイルに触れる操作（`session_prompt` の 2 チャネル配送・ライブペイン検知・`session_remove`）を `AgentBackend`、明示モデルの可用性確認を `AgentModelProbe` で抽象化する。`mode` → チャネルの振り分けと、検証成功後だけ state / queue を変更する順序をユニットテストで網羅する。 |
| `usecase/agent` | Agent CLI の導入・MCP capability 判定に加え、モデル可用性の三値（利用可能 / 利用不能 / 検証不能）、fail-closed の共通エラー化、CLI 出力の純粋パース、本番 `CliAgentModelProbe` を担う。PATH 上の CLI を検査する既存 `CommandRunner` と同じ capability-check use case に置き、process runner を fake に差し替えて argv・終了状態・出力上限・UTF-8・タイムアウト判定を計測対象のユニットテストで網羅する。stdout / stderr reader の結果待ちと直接子プロセスの終了待ちは個別の deadline で打ち切る。stderr はパイプ詰まりを防ぐため保持量を制限して読み捨て、その本文を MCP エラーに含めない。Unix では probe を独立 process group で起動し、timeout または reader 失敗時に group 全体の終了を試みる。非 Unix で終了を試みる対象は直接子だけで、子孫と detached reader の終了は保証しないが、MCP request 自体の待ち時間は deadline で制限する。 |
| `usecase/issue`・`usecase/memory`・`usecase/session` ほか | tool が呼ぶビジネスロジック。MCP 固有の知識は持たない。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase → infrastructure`。MCP 層は
presentation に閉じています（[2. アーキテクチャ](../02-architecture.md) 参照）。

## 対応 tool 一覧

`tools/list` で以下の 26 tool（issue 6 + memory 4 + session 14 + オーケストレーション 2）を公開します。結果はいずれも JSON テキストで
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
| `session_create` | `name` | `agent_cli` / `model` | 作成されたセッション（`name` / `root` / `worktrees`） |
| `session_list` | — | — | セッション配列（各要素に `name` / `display_name` / `origin` / `started_from` / `root` / `created_at` / `worktrees`） |
| `session_status` | — | — | セッション配列（各要素に `name` / `display_name` / `origin` / `started_from` / `root` / `agent_phase` / `worktrees`。各 worktree に `status` / `dirty` / `merged`）（[挙動](#session_status-の挙動)） |
| `session_prompt` | `name` / `prompt` | `mode`（`auto` / `queue` / `live`、既定 `auto`） / `agent_cli` / `model` | `{ "name": "…", "delivered_to": "queue" \| "live", "detail": "…", "agent": { "cli": "…", "model": "…" } }`（`agent` は `agent_cli` / `model` 指定時のみ。[挙動](#session_prompt-の挙動)） |
| `session_complete` | `message` | — | `{ "session": "…", "reported_to": "親 session 名" \| ":root", "delivered_to": "queue" \| "live", "detail": "…" }`。セッション内限定（[挙動](#session_complete-の挙動)） |
| `session_pr` | `name` | — | `{ "name": "…", "root": "…", "merged": bool, "pr": [{ "number": N, "url": "…", "state": "open" \| "merged" }] }` |
| `session_remove` | `name` | `force` | `{ "name": "…", "removed": bool, "dirty": [worktree…] }`（[挙動](#session_remove-の挙動)） |
| `session_note_get` | — | — | `{ "name": "…", "note": string? }`（現在のセッションのメモ。未設定なら `null`）。セッション内限定 |
| `session_note_update` | `note` | — | `{ "name": "…", "note": string? }`（保存後のメモ。空文字でクリア＝`null`）。セッション内限定 |
| `session_todo_list` | — | — | `{ "name": "…", "todos": [{ "text": "…", "done": bool }] }`（現在のセッションのチェックリスト）。セッション内限定 |
| `session_todo_add` | `text` | — | 追加後の `{ "name", "todos" }`（`text` は trim・非空必須）。セッション内限定 |
| `session_todo_update` | `index` | `done` / `text` | 更新後の `{ "name", "todos" }`（`done` と `text` の少なくとも一方が必須。範囲外 index はエラー）。セッション内限定 |
| `session_todo_remove` | `index` | — | 削除後の `{ "name", "todos" }`（範囲外 index はエラー）。セッション内限定 |
| `session_decision_list` | — | — | `{ "name": "…", "decisions": [{ "at": RFC3339, "text": "…" }] }`（意思決定ログ）。セッション内限定 |
| `session_decision_log` | `text` | — | 追記後の `{ "name", "decisions" }`（`at` はサーバが現在時刻を付与。`text` は trim・非空必須）。セッション内限定 |
| `session_delegate_issue` | `number` | `name` / `agent_cli` / `model` | `{ "issue": N, "title": "…", "session": "…", "root": "…", "worktrees": […], "delivered_to": "queue" }`（[挙動](#session_delegate_issue-の挙動)） |
| `session_delegate_brief` | `brief` | `name` / `agent_cli` / `model` | `{ "session": "…", "root": "…", "worktrees": […], "delivered_to": "queue" }`（[挙動](#session_delegate_brief-の挙動)） |

- `status` は `todo` / `in-progress` / `done`、`priority` は `high` / `medium` / `low`、`type`（memory）は `user` / `feedback` / `project` / `reference`。
- **`memory_save` は upsert 1 本**です。`name` が既存なら**渡したフィールドだけを部分更新**（未指定は保持、`created_at` も保持）、無ければ新規作成（このとき `title` 必須）。`name` は与えた文字列をスラッグ化して識別子にします。別途の `memory_update` tool はありません（body だけ直したいときは `name` と `body` だけ渡せば type 等は保たれます）。
- **一覧は検索の特殊形として統合**しています。`issue_search` / `memory_search` は `query` を省略すると全件を返し（空クエリはすべてに一致）、`query` を与えると全文検索に絞り込みます。フィルタ（`status` / `type` など）は `query` の有無にかかわらず併用できます。別途の `issue_list` / `memory_list` tool はありません。
- `dependson` はブロックする先行条件、`related` はブロックしない関連、`parent` は所属（Epic ⊃ サブタスク）、`milestone` は束ね。`issue_search` は `parent` / `milestone` でも絞り込めます。
- `issue_update` の `parent` / `milestone` は三状態です: 省略すると変更なし、**`null` を明示すると解除**、値を渡すと設定します。
- `issue_get` / `issue_update` / `issue_delete` は、同番号の markdown が複数あると衝突した exact path を列挙する tool error を返し、どの sibling も暗黙に選択・変更しません。明示的な repair/migration は [番号 identity と重複修復](../data/03-issues.md#番号-identity-と重複修復) の手順に従います。
- `issue_search` は CLI と同じく **`dependson` がすべて `done` の issue を `ready: true`**
  とし、未達の依存番号を `unmet_deps` に入れて返します（着手可能なタスクの判別用）。
- `ready: true`（引数）を渡すと着手可能な issue だけに絞り込みます。
- `issue_to_prompt` は issue を **そのまま実行できるエージェント向けプロンプト**に整形して返します
  （実装手順・status 更新の指示と issue 本文を含む）。プロンプトはリポジトリ非依存の文言で、特定言語の
  コマンドや usagi 固有のパスは埋め込みません（リポジトリ側の規約ドキュメントに従わせます）。
  `issue_to_prompt(number)` → `session_create(name)` → `session_prompt(name, prompt)` の 3 手を
  1 回で行うのが **`session_delegate_issue`** です（[挙動](#session_delegate_issue-の挙動)）。プロンプトを
  自分で調整したい／既存セッションに載せたい場合はこの primitive 3 つを直接使います。
- **プロンプトに埋め込む status 指示は「単一書き手」ライフサイクルに沿います**（[.agents/workflow.md](../../../.agents/workflow.md)）。
  委譲された session に対し、**着手時に自 worktree で `status = in-progress`**、**PR を開く前に自 worktree で
  `status = done` をコミットして実装差分と同じ PR に載せる**（別コミットでよい）ことを指示します。`done` を
  反映できるのはその session の枝だけで、マージで初めて基点ブランチの issue が `done` になります。root は
  `status` を書かず、生存 session（`session_list` / `session_status`）を in-progress の実効シグナル、
  `session_status.merged` / `session_pr` を `done`（マージ）の検知に使います。
- `session_delegate_issue` は「issue を新しいセッションに委譲して着手させる」というオーケストレーションの
  定番手順を 1 tool にまとめたものです。issue をプロンプト化し、`name`（既定 `issue-<番号>`）でセッションを
  作成し、そのプロンプトを起動時キューに積むまでを行います（[挙動](#session_delegate_issue-の挙動)）。
- `session_delegate_brief` は「事前 issue を要さない**起源フロー**」の入口です。root は自由記述のブリーフを渡すだけで、
  tool が `name`（既定は次に空く `triage-<n>`）でセッションを作成し、ブリーフをトリアージ session 用の定型指示で
  ラップして起動時キューに積みます（[挙動](#session_delegate_brief-の挙動)）。root は git 追跡下の issue を書けない
  （[ルートでの書き込みガードレール](#ルートでの書き込みガードレール)）ため、作業はこの起源 session が worktree 内で
  `issue_create` して PR することで生まれます。
- `session_create` は `name` をセッション名として `<root>/.usagi/sessions/<name>/` に worktree を生成します
  （各リポジトリで切るブランチは `usagi/<name>`）。空・パス区切り文字を含む名前は拒否し、
  既存のセッション名は重複エラーになります（CLI と同じ検証）。`session_list` は `state.json` を読むだけの
  軽量クエリで、on-disk の reconcile は行いません。
- `session_create` は worktree を生成するだけで、動作中の TUI の[集中](../design/home/02-layout.md#集中closeup)には
  入りません（`usagi mcp` は TUI を操作できない別プロセスのため）。TUI から作成したときは作成完了後にその
  セッションへ自動で集中しますが、MCP 経由の作成はホーム画面の一覧にバックグラウンドで反映されるだけで
  カーソルは動きません。
- **`session_create` / `session_delegate_issue` / `session_delegate_brief` は任意で `agent_cli` / `model` を受け取り、そのセッション単位で
  起動するエージェント CLI・モデルを固定できます**（ワークスペースの実効設定 `agent_cli` より優先）。コーディネータが
  「軽いタスクは小さいモデル、重い設計は大きいモデル」とタスクごとに振り分けるための口です。指定は `state.json` の
  `SessionRecord.agent`（`cli` / `model`）に記録され（正本は [data/02-workspace.md](../data/02-workspace.md#statejson)）、
  そのセッションのエージェントペイン起動（自動 spawn・ペイン復旧・集中からの起動）時に適用されます。
  - `agent_cli` は `claude` / `codex` / `codex-fugu` / `gemini` / `agy`（大文字小文字・表示名も可、`AgentCli::from_name`）。
    未知の値は実行エラー（`isError: true`）で、セッションは作成されません。
  - `model` は前後空白を除去したうえで、実効 `agent_cli` が現在列挙するモデルとの**完全一致**を確認してから保存します。
    Codex は `codex debug models`、Antigravity は `agy models` の動的一覧を使うため、静的 allowlist は持ちません。
    一覧に無いモデル、一覧取得の失敗・タイムアウト、または検証 probe が未対応の CLI への明示モデルは実行エラーとなり、
    セッションを作成しません。現時点では安全な一覧取得手段がない Claude / sakana.ai に加え、動的一覧取得をまだ実装して
    いない Gemini が未対応です。これらの CLI は `model` を省略して CLI 自身の既定モデルを使います。空文字は無指定として
    落とします。`model` だけを指定した場合は、検証に使ったワークスペースの実効 CLI を `agent.cli` に固定し、
    明示モデルと pair で保存します。
  - どちらも未指定なら従来どおり、ワークスペースの実効 `agent_cli` と各 CLI の既定モデルにフォールバックします。
- **セッションスクラッチパッド（`session_note_*` / `session_todo_*` / `session_decision_*`）**は、いずれも
  **その MCP プロセスが動いているセッション自身**を対象にします（引数でセッション名を取らず、worktree のパスから
  現在のセッションを導出。ワークスペースルートで起動した場合は実行エラー）。3 区画は用途が分かれています:
  - **note**（自由記述メモ）: 経緯・リンク・覚え書き。`session_note_update` は verbatim に保存し、末尾空白を trim、
    空文字でクリアします。
  - **todo**（軽量チェックリスト）: そのセッション内の使い捨てタスク。`text` は trim・非空必須、`done` の既定は未チェック。
    **git 管理の issue ストア（`.usagi/issues/`）とは別物**で、起票するほどでない覚え書きに使います。
  - **decision**（意思決定ログ）: 追記専用。`session_decision_log` は「なぜその方針にしたか」を**サーバが付与する
    現在時刻（`at`）付きで**追記します。コーディネータが `session_decision_list` で判断根拠を transcript 抜きに追えます。
  - いずれも保存先は `state.json`（マシンローカル・git 管理外）で、正本は [data/02-workspace.md](../data/02-workspace.md#statejson)。
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
  ではなく**ルート行のコーディネータ**へ任意のプロンプトを配送できます
  （[ルート行へのプロンプト送信](#ルート行コーディネータへのプロンプト送信)）。
- `session_complete` は現在のセッションを作成した呼び出し元を `started_from` から自動解決し、完了報告を push します
  （[挙動](#session_complete-の挙動)）。
- `session_remove` はセッションの全 worktree とブランチを破棄し、コピーされたファイルとエージェントの会話履歴を
  消して `state.json` から削除します。`usagi clean` が起動するバックグラウンドエージェントは、このツールで
  放置セッションを片付けます（[挙動](#session_remove-の挙動)）。
- `session_list` / `session_create` の各 worktree 要素は `path` / `branch` / `head` / `primary` / `status` を持ちます
  （保存フォーマットの正本は [data/02-workspace.md](../data/02-workspace.md#statejson)）。
- `session_list` / `session_status` の各セッション要素は `origin`（`human` / `mcp` / `unknown`）を持ち、そのセッションを
  人が TUI で起動したのか、エージェントが MCP で起動したのかを区別できます（正本は [data/02-workspace.md](../data/02-workspace.md#セッションごとsessionrecord) の `origin`）。
- `session_list` / `session_status` の各セッション要素は `started_from`（親セッション名、無ければ `null`）を持ち、その
  セッションが**どのセッションから開始されたか**が分かります。エージェントがあるセッションの中から `session_create` /
  `session_delegate_issue` / `session_delegate_brief` を呼ぶと、その親セッション名が記録されます（ルートから作成した場合は `null`）。これにより
  コーディネータは自分が委譲した子セッションの系譜を再構成できます（正本は [data/02-workspace.md](../data/02-workspace.md#セッションごとsessionrecord) の `started_from`）。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## `session_status` の挙動

`session_status` は、コーディネータ役のエージェントが**委譲先セッションの進捗をポーリング**して、子の完了や
PR のマージを検知し、`session_remove` → 次の issue 委譲へと自律ループを回すための読み取り専用 tool です。
各セッションについて次を返します。

| フィールド | 内容 |
|---|---|
| `origin` | セッションを**誰が起動したか**。`human`＝人が TUI で作成、`mcp`＝エージェントが `session_create` / `session_delegate_issue` / `session_delegate_brief` で作成、`unknown`＝この項目を持たない古い `state.json` のセッション（未記録）。作成時に一度だけ記録し、同期では書き換えない。 |
| `started_from` | セッションが**どのセッションから開始されたか**（親セッション名）。エージェントがあるセッションの中から作成すると親名が入り、ルートから作成すると `null`。作成時に一度だけ記録し、同期では書き換えない。 |
| `agent_phase` | セッションの root worktree に記録されたエージェントの lifecycle phase（`ready` / `running` / `waiting` / `ended` / `exited`）。`ended` はターン完了、`exited` は Agent process 終了です。ペインが一度も起動していない（またはペインが死んで phase がクリアされた）場合は `none`。TUI のバッジを駆動するのと同じ agent phase ファイルを読む。 |
| `worktrees[].status` | worktree の git 状態（`new` / `dirty` / `local` / `pushed` / `synced`）。`usagi status`（[`state.json`](../data/02-workspace.md#statejson)）と同じ分類。 |
| `worktrees[].dirty` | 未コミットの変更がある（`status == dirty`）。 |
| `worktrees[].merged` | デフォルトブランチがこの worktree の内容をすべて含む＝マージ済み（`status == synced`）。 |

- **読み取り専用・軽量**です。`state.json`（直近の同期で記録した worktree 状態）と agent phase ファイルだけを
  読み、**git を起動しません**。値の鮮度は直近の[ワークスペース同期](../data/02-workspace.md#statejson)
  （稼働中の TUI がバックグラウンドで実行）に一致します。ポーリング用途で繰り返し呼んでも安価です。
- `agent_phase` の `ended` は「子エージェントがターンを完了した」ことを示します。一方 `exited` は Agent process が
  消えたという liveness 情報だけで、成功を意味しません。コーディネータは `ended` / `exited` を契機に issue・PR・完了イベントを
  確認し、成功なら `session_remove`、未完了なら再投入または人へのエスカレーションを行います。`merged` は
  「作業がデフォルトブランチに取り込まれた」ことを示します。
- `merged` は worktree 状態から導出するため、リモートでマージされた反映にはローカルの
  `origin/<default>` が最新である必要があります（`usagi status` と同じ制約。GitHub には問い合わせません）。
- セッションが 1 件も無ければ空配列 `[]` を返します。並び順は `session_list` と同じ（ホーム一覧の表示順）。

## `session_prompt` の挙動

`session_prompt` は対象セッションのエージェントにプロンプトを渡す唯一の tool です。その場ではエージェントの
応答を返さず（起動もしません）、**2 つの配送チャネル**のいずれかにプロンプトを託します。`usagi mcp` は
動作中の TUI に手を伸ばしてペインを操作できない別プロセスのため、どちらもファイル経由でキューします。

| チャネル | キュー先 | 配送タイミング |
|---|---|---|
| 起動時キュー（queue） | [`agent-prompts/`](../data/01-global.md#agent-prompts) | 明示 `mode="queue"` の prompt は、そのセッションのエージェントペインを**次にフレッシュ起動するとき**に、エージェントの**最初のメッセージ**として渡す。対象に **live agent ペインが検知される場合**、`mode="queue"` は fresh launch が来ず滞留するため tool error で拒否する（稼働中 Agent への追送は `auto`/`live`、作り直すなら remove）。`auto` が live pane 無しと判定して queue を選んだ prompt だけは、TUI が既存 Agent pane を持つ場合（既存 Agent pane の復旧後）、その Agent の**次のメッセージ**として渡せる。設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が有効なら、TUI 稼働中は人がペインを開くのを待たない（[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)） |
| ライブキュー（live） | [`agent-live-prompts/`](../data/01-global.md#agent-live-prompts) | **すでに起動中のエージェントペイン**へ、動作中の TUI の監視スレッドが「貼り付け → Enter」で流し込む。設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が有効なら、対象セッションにライブペインが**無い**場合でも TUI がこれを検知してペインを**バックグラウンドで自動 spawn** し、キュー分を最初のメッセージとして着手させる（[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)） |

どちらを使うかは `mode` 引数で決めます（省略時 `auto`）。

- **`auto`（既定）**: セッションにライブなエージェントペインが検知できれば **live**、なければ **queue** を選びます。
  ペインの有無は、起動中の TUI が発行する [live-pane マーカー](../data/01-global.md#agent-live-panes)で判定します。
  マーカーは発行した TUI の pid を刻むため、その TUI が終了・クラッシュした後は（マーカーが残っていても）読み手が
  死んだ pid を検知して「ライブペイン無し」と判定します。**agent phase ファイルは使いません**（phase はアイドルでも
  `ready` を示し、TUI 終了後も残留するため、それを live 判定に使うと誰も流し込まない live チャネルへ振り分けてしまう）。
  **呼び出し側はエージェントが起動中かどうかを知らなくてよい**のが利点です。
  なお `auto` はマーカーで消費者の有無を正しく判定するため、ペインの無いセッションへ live 振り分けすることはありません。
  TUI 不在中は daemon が Agent を保持していても live-pane マーカーが無いため queue を選びます。この `auto` の
  フォールバック記録に限り、次回 TUI が既存 Agent pane を持つと fresh launch を待たずに引き渡します（典型例は
  daemon 所有 Agent への再 attach）。監視上 `running` / `waiting` の間は待ち、`ready` / `ended` / phase-less で配送します。
  `exited` は Agent process が無く bare shell だけ残り得るため live consumer から外し、queue dispatcher が fresh spawn します。
  phase 非対応 Agent は状態を断定できないため、通常の live prompt と同じ best-effort 配送です。
  ただし**明示 `mode="live"`** はマーカーを見ずに常にライブキューへ積むため、ペインが無ければ滞留します（下記 `live`）。
  この滞留は、TUI 側の権威的なペイン生存判定（PTY の生死）に基づく自動起動が spawn して救済します
  （上記ライブキュー行・[4. オーケストレーション](../04-orchestration.md#キュー済みプロンプトの自動起動)）。
- **`queue`**: ライブペインの有無にかかわらず、常に起動時キューへ入れ、**次の fresh Agent の最初のメッセージ**として
  渡します。既存の live Agent へは引き渡しません。自動起動が有効でも live Agent が残っている間は queue で待ち、
  Agent が無ければ新規 spawn します。
- **`live`**: 常にライブキューへ入れます（ペインがまだ無ければ、開くまでキューで待ちます。設定
  `autostart_queued_prompts` が有効ならペインの無いセッションでは自動 spawn で救済されます）。

任意で `agent_cli` / `model` を渡すと、対象セッションの**次回以降の agent 起動**に使う CLI / モデルの
上書きを、プロンプト配送前に保存します。明示 `mode="queue"` の prompt と保存した起動設定は、既存 Agent には適用せず、
次にフレッシュ起動する Agent へ一緒に渡します。`auto` が queue を選んだ場合、既存 Agent pane が無ければ
同様に指定 Agent を自動起動します。既存 Agent pane へ今回の prompt を引き渡した場合も、保存した
CLI / モデルは稼働中プロセスを変更せず、そのペインを終了して次にフレッシュ起動したときから適用します。
`mode="live"`（または `auto` が live を選んだ場合）も今回の入力は現在の Agent へ配送し、起動設定は次回から適用します。
`agent_cli` は `session_create` / `session_delegate_issue` / `session_delegate_brief` と同じ
`claude` / `codex` / `sakana.ai` / `gemini` / `antigravity` を受け取り、インストール済みかつ MCP 対応の CLI だけを許可します。
`agent_cli` または `model` の片方だけを渡した場合、もう片方の既存上書きは保持します。`model` は前後空白を除去して保存し、
空白だけならモデル上書きをクリアします。`name=":root"` はセッションではなくルート行の
配送先なので、`agent_cli` / `model` は指定できません。

`name=":root"` 以外への `session_prompt` は配送 mode や live pane の有無にかかわらず、更新後の
組み合わせ（引数を省略した側は既存上書き、CLI 上書きが無ければワークスペースの実効 `agent_cli`）に
明示モデルがあれば、state 更新といずれかのプロンプトキューへの追記より前に可用性を毎回検証します。稼働中
pane への live follow-up も、pane が消失するとライブキューが fresh launch の最初の入力になり得るため検証を省略しません。
明示モデルは検証対象の CLI と pair で保存します。`model` だけを指定した場合は既存の CLI 上書きを保持し、それも
無い場合は検証に使ったワークスペースの実効 CLI を `agent.cli` に固定します。旧い `state.json` に `model` だけが残っている場合も、
同じく実効 CLI との pair に正規化してからキューへ追記します。モデルが利用不能または
検証不能なら `isError: true` を返し、agent 上書きと両方のプロンプトキューを変更しません。エラーになった保存済み上書きは、
空白の `model` を明示してクリアできます。`model` を持たず CLI の既定モデルを使う場合は検証を要しません。対応 CLI と判定方法は上の
[対応 tool 一覧](#対応-tool-一覧) を正本とします。

自動 spawn は queue claim 後に authoritative な `state.json` を読み直すため、TUI の HomeState 反映が 1 tick 遅れていても
古い CLI / model では起動しません。この検証時点は MCP が state / queue を更新する直前です。キュー後に TUI が遅延して fresh agent を spawn する時点では
再検証しないため、キュー中に CLI のモデル一覧が変わった場合の起動時可用性までは保証しません。

返り値の `delivered_to` に、実際に配送したチャネル（`"queue"` / `"live"`）が入るため、`auto` を使っても
どちらに届いたかが確認できます。`detail` にはチャネルごとの確認メッセージが入ります。**`delivered_to: "live"`
は「ライブキューへ追記した」ことを表し、その場で稼働ペインへ届いたことを保証しません**（実配送は起動中 TUI の
監視スレッドのみが行う）。そのため `live` 送信時の `detail` は、送信時点でライブペインが検知できたか否かを明示し、
検知できなければ「開くまで配送されない／確実に動かすなら `mode="queue"` を使う」旨を案内します。

### ルート行（コーディネータ）へのプロンプト送信

`name` に予約名 **`:root`** を渡すと、セッションではなく**ワークスペースのルート行**（[`⌂ root`](../04-orchestration.md#用語)）を
配送先にできます。通常のセッションを介さず、ルート行で動くコーディネータ役のエージェントへ任意のプロンプトを
送るための低レベルな経路です。

- `:root` はルート行の作業ディレクトリ（= workspace root）へ配送します。ルート行はどのセッションにも属さないため、
  `:root` の解決に**セッションの存在は不要**です。配送チャネルの選び方（`mode` / `auto` の live・queue 判定）は
  通常のセッション宛と同じで、コーディネータのペインが起動中なら **live** で即座に流し込み、起動していなければ
  起動時キューへ積みます。`auto` のフォールバック記録は既存の対象 Agent への監視 tick 配送または新規 spawn で
  着手し、明示 `queue` は次の fresh Agent まで待ちます。
- 予約名は先頭に `:` を付けた `:root` です。セッション名は git ブランチ / ディレクトリ名になるため先頭 `:` を
  持たず、`:root` が実在のセッションと衝突・混同することはありません。
完了報告には、配送先を自動解決する [`session_complete`](#session_complete-の挙動) を使います。

- **起動時キュー**にキューした明示 `mode="queue"` のプロンプトは、[集中](../design/home/02-layout.md#集中closeup)から
  `agent` を実行するか、自動起動によって**エージェントペインを新規 spawn する**ときに 1 回だけ消費されます。
  `auto` が queue へフォールバックした記録も通常は同じですが、既存 Agent pane がある場合は（典型的には daemon Agent への
  再 attach 後）、監視上 `ready` / `ended` / phase-less の tick に次のメッセージとして渡します。`exited` は bare shell を
  consumer として扱わず fresh spawn へ戻します。どちらも Agent 起動後の stdin へ paste + Enter で渡すため、
  prompt 本文を shell argv に含めません。自動配送の input handle 送信が失敗した場合は、以前の retry 履歴を保ったまま
  attempt と上限付き backoff を更新し、起動時キューへ戻します。
  daemon terminal への durable prompt input は request id 付き `Keys` を使い、PTY write 後の `InputResult` ACK を待ちます。
  missing terminal、PTY write failure、ACK timeout では同じ履歴を引き継いで上記 retry を更新・復元します。timeout は daemon が書き込み済みで
  ACK だけ失われた場合も含むため、再試行は at-least-once です。失敗後の `Kill` は `Killed` ACK だけを teardown 証明とし、
  ACK 不明なら同じ TUI run の unattended retry を停止します。次回起動も daemon registry が既存 terminal を示す間は fresh Agent を
  spawn しません。設定を OFF にすると監視 tick の引き渡しと自動 spawn は行わず、人が Agent
  ペインをフレッシュ起動するまでキューに残ります
  （[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)）。
- **ライブキュー**のプロンプトは、対象セッションに live agent ペインがある場合 TUI の監視 tick（約 200 ms 間隔）で
  配送されます。複数回送ったプロンプトは追記順に、各 1 回だけ配送されます。配送は「取り出し → 書き込み」の順で
  行い、PTY への書き込みが失敗したプロンプト（および同じ tick でそれ以降にあった未配送分）はライブキューの
  **先頭へ戻して**次の tick で再試行するため、キュー済みと返答したのに黙って失われることはありません。書き込みは
  端末の paste と同じ扱いで、対象プログラムが bracketed paste mode を有効にしているときはプロンプト全体を
  bracketed paste で包んでから Enter を送り、複数行プロンプトが途中で複数回 submit されるのを避けます。
  ライブペインが**無い**セッション（明示 `mode="live"` を live ペインの無いセッションへ送った場合など）に
  ライブキューが残っているときは、設定 `autostart_queued_prompts`（既定 ON）が有効なら自動起動が起動時キューと同様にこれを拾い、
  fresh agent の最初のメッセージとして spawn します（滞留の防止。[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)）。
- 作業はどちらのチャネルでもセッションのブランチ（worktree）上で隔離されます。
- `prompt` にはサイズ上限（128 KiB）があります。超えるとチャネルへ書き込む前にツールエラーとして拒否します。

## `session_complete` の挙動

`session_complete(message)` は、現在のセッションを作成した呼び出し元へ完了報告を push します。agent は配送先を指定しません。
作成時に一度だけ保存した `SessionRecord.started_from` を MCP サーバが読み、親 session 名があればその session、無ければ
`:root`（ワークスペースのルート行）を配送先にします。これにより、ネストした委譲でも完了報告が直近の呼び出し元へ戻ります。

- 呼び出せるのは session worktree 内だけです。
- 報告には現在 session 名が自動で付与され、`Session "<name>" completed:` に続けて `message` が配送されます。
- 配送先が親 session なら、その親に保存された明示モデルを完了報告キューの追記前に再検証します。旧い記録で CLI が無ければ、検証したワークスペースの実効 CLI を親の `agent.cli` に固定します。モデルが利用不能または検証不能なら tool エラーとし、state と両方のプロンプトキューを変更しません。配送先が `:root` なら session 単位の上書きが無いためこの検証は行いません。
- 配送は `session_prompt` の `auto` と同じです。呼び出し元の live agent ペインがあれば live、無ければ次回起動用キューへ送ります。
- 戻り値の `reported_to` で解決された宛先、`delivered_to` で実際の配送チャネルを確認できます。
- `started_from` だけを return address として保持するため、agent 固有の会話 ID や MCP client 情報は永続化しません。

## `session_delegate_issue` の挙動

`session_delegate_issue` は、コーディネータ役のエージェントが最も多用する「issue を新しいセッションに委譲して
着手させる」手順を 1 呼び出しにまとめた**オーケストレーション tool**です。次の 5 ステップを順に行います。

1. `agent_cli` / `model` 引数が指定された場合、CLI がインストール済みかつ MCP 対応であり、明示モデルがその CLI で現在利用可能かを検証する（検証不能も含め、エラーがあれば早期リターン）。
2. `issue_to_prompt(number)` で issue を実行プロンプトに整形する（issue が無ければ実行エラー）。
3. **基点ブランチへのコミット検証**: 委譲する issue ファイル（`.usagi/issues/<slug>.md`）が、委譲先ワークツリーの基点コミット（プロジェクトの default_branch 設定に応じた、`origin/main` または `main`）にすでにコミットされているかを検証する。コミットされていない場合はエラーを返します。
4. `session_create(name)` でセッションを作成する（`name` 既定は `issue-<番号>`。名前が既存なら重複エラー）。
5. `session_prompt(name, prompt, mode=queue)` でそのプロンプトを起動時キューに積む。

- 新規作成したセッションには live なエージェントペインが存在しないため、配送は常に**起動時キュー**（`queue`）です。
  返り値の `delivered_to` も常に `"queue"` になります。
- **基点コミット検証の理由**: 新しいセッションワークツリーは基点ブランチ（既定 `main`）から切られるため、未コミットの issue はセッションのブランチに乗らなくなります。これにより、セッション内から issue の `status` 変更などができなくなる（#104）のを防ぐため、あらかじめコミット・マージ済みであることを検証して誤運用を防ぎます。
- 設定 [`autostart_queued_prompts`](../05-settings.md#設定項目)（既定 ON）が有効で TUI が稼働していれば、委譲した
  セッションの agent ペインは人が開かなくても**バックグラウンドで自動起動**され、キュー済みプロンプトに着手します
  （[4. オーケストレーション#キュー済みプロンプトの自動起動](../04-orchestration.md#キュー済みプロンプトの自動起動)）。OFF なら次に人がそのペインをフレッシュ起動するまでキューで待ちます。
- 既存の 3 tool（`issue_to_prompt` / `session_create` / `session_prompt`）を合成サーバ（`usagi.rs`）が順に呼び、
  その前段で基点コミット検証だけを足します。採番・セッション生成・キューなどの挙動は primitive と完全に一致します。
- primitive はそのまま残っています。**プロンプトを手で調整したい**、**既存セッションに載せたい**、**live 送信したい**
  といったケースでは `issue_to_prompt` → `session_prompt`（`mode` 指定）を直接使ってください。
- 途中のステップが失敗すると（issue 不在・未コミット・セッション名重複・未知の `agent_cli` など）その時点でツールエラーを返します。
  `agent_cli` の検証は最も最初に行うため、未知の CLI 名を渡してもセッションは作られません。
- 任意で `agent_cli` / `model` を渡すと、委譲先セッションを**指定 CLI・指定モデルで起動**できます（`autostart_queued_prompts`
  による自動起動時にそのまま適用）。指定の意味とフォールバックは上の [対応 tool 一覧](#対応-tool-一覧) の説明と同じです。

## `session_delegate_brief` の挙動

`session_delegate_brief` は、root が git 追跡下の issue を直接作らずに作業を生み出すための**起源フロー**です。
事前 issue は不要で、コーディネータ役のエージェントは自由記述の `brief` を渡すだけです。tool は次の 4 ステップを順に
行います。

1. `agent_cli` / `model` 引数が指定された場合、CLI がインストール済みかつ MCP 対応であり、明示モデルがその CLI で現在利用可能かを検証する（検証不能も含め、エラーがあれば早期リターン）。
2. `name`（省略時は既存 session と衝突しない次の `triage-<n>`）でセッションを作成する。
3. `brief` をトリアージ session 用の定型指示でラップする。定型指示は「調査して issue を起票する」「必要なら実装 issue に分割する」「issue/backlog 変更をこの session のブランチで PR する」「root に git 追跡下の issue 作成・編集を依頼しない」を含みます。
4. `session_prompt(name, wrapped_brief, mode=queue)` でそのプロンプトを起動時キューに積む。

- 新規作成したセッションには live なエージェントペインが存在しないため、配送は常に**起動時キュー**（`queue`）です。
  返り値の `delivered_to` も常に `"queue"` になります。
- 起源 session は worktree 内で `issue_create` / `issue_update` を実行できます。作成した issue はその session の
  ブランチに乗り、PR をマージすると `main` の backlog に現れます。以後 root はその committed issue を
  `session_delegate_issue` で遂行 session に委譲できます。
- `name` を省略すると `triage-1`, `triage-2`, ... のうち既存 session と衝突しない最小の名前を選びます。
  明示した `name` が既存なら、通常の `session_create` と同じ重複エラーになります。
- 任意で `agent_cli` / `model` を渡すと、起源 session を**指定 CLI・指定モデルで起動**できます。指定の意味と
  フォールバックは上の [対応 tool 一覧](#対応-tool-一覧) の説明と同じです。

## `session_remove` の挙動

`session_remove` はセッションを物理的に破棄します。CLI / TUI のセッション削除（[`session remove`](02-tui.md#session)）と
同じ `usecase/session::remove` を呼ぶため、挙動は一致します。

- `state.json` に durable removal marker を保存してから全リポジトリの worktree とセッションブランチを取り外し、コピーされたファイルを削除します。全 Git teardown が成功した後だけ、各 worktree のエージェント会話履歴（例: Claude のトランスクリプト）と usagi が記録する agent phase・PR・prompt queue・pane context を消し、最後に session record を落とします。会話履歴を消す対象 CLI は、ワークスペースの実効設定（`agent_cli`）から解決します。
- **未コミットの変更がある worktree は、既定では削除しません**。この場合 `removed: false` を返し、ブロック要因の
  worktree を `dirty` 配列で示します。`force: true`（任意引数、既定 `false`）を渡すとその変更を破棄して削除します。
- 存在しないセッション名は実行エラー（`isError: true`）になります。
- teardown / cleanup の途中失敗も実行エラーになり、session identity と recovery context は保持されます。`session_status` の `removal`（`git_teardown` / `context_cleanup`）とエラーの案内に従って原因を直し、同じ `session_remove` を再実行すると残りから冪等に再開します。通常セッションの `removal` は `null`、所有権不明の既存 orphan は `orphaned` で、自動 force delete されません。

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
| すべての `session_*` / `session_delegate_issue` / `session_delegate_brief` | 実行可 | 実行可 |

- 拒否は tool 実行エラー（`isError: true`）として返し、「root では実行できない・セッション worktree 内で行うこと」を
  案内します。エージェントはこのテキストを読んで、`session_create` / `session_delegate_issue` / `session_delegate_brief` でセッションを開いてから
  書き込むよう自己修復できます。
- 読み取り・整形（`issue_get` / `issue_search` / `issue_to_prompt` / `memory_get` / `memory_search`）と、
  オーケストレーションに必要な `session_*`・`session_delegate_issue`・`session_delegate_brief` は root でも許可します。既存の issue を
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
  - **手順の統合**: 頻出のオーケストレーション（issue→新セッション委譲）を `session_delegate_issue` の 1 呼び出しに、
    事前 issue を要さない起源フロー（brief→トリアージ session）を `session_delegate_brief` の 1 呼び出しにまとめる。
    ただし primitive（`issue_to_prompt` / `session_create` / `session_prompt`）は残し、細かい制御が要るときはそれらを使う。
  - issue/memory の CRUD は「エージェントが所有するデータストアの素の操作」で、無理に融合すると機能が隠れるため
    残しています。CLI（人間向け）とは IF を分けて最適化しています。
