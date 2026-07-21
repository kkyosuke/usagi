# 7. MCP サーバ（agent 入口面）

> [ドキュメント目次](README.md) ｜ ← 前へ [6. 開発規約](06-conventions.md) ｜ 次へ → [8. coverage exclusion inventory](08-coverage.md)

`usagi mcp` は AI エージェント向けの入口面で、stdio 上の JSON-RPC 2.0 で tool と resource を
公開する。面の責務・経路・daemon を権威とする反映フローの設計判断は
[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md) が正本で、本章は現在の
ビルドが公開する wire 面をまとめる。

## 目次

- [起動と経路](#起動と経路)
- [プロトコルとライフサイクル](#プロトコルとライフサイクル)
- [JSON-RPC メソッド](#json-rpc-メソッド)
- [tool 面](#tool-面)
- [resource 面](#resource-面)
- [orchestration ガイド](#orchestration-ガイド)

## 起動と経路

`usagi mcp` は合成ルートが stdin/stdout を束ねて serve ループを回す（エージェントが spawn する
stdio プロセスで、CLI からは隠している）。起動時に daemon へ接続し、停止中なら autostart する。
daemon に接続できなければ stdio serve ループを開始しない（[2. アーキテクチャ](02-architecture.md)、
[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)）。

daemon-provisioned MCP child は private caller credential を IPC に forward する。dispatch/agent tool と `user_decision_*` は
この credential を持つ live daemon Agent runtime だけが利用でき、手動の `usagi mcp` や credential の無い
MCP caller は `ownership_unknown` で fail-closed となる。caller identity、session 名、cwd、path を
tool payload や環境から補完して認可することはない。

Codex を daemon が起動するときは、注入した `usagi` stdio server だけにこの credential と `USAGI_HOME` を
`env_vars` で forward し、server の tool approval mode を `approve` にして各 MCP call の対話確認を省略する。
認可を省略するものではなく、daemon は credential、live runtime、dispatch
binding を引き続き照合するため、credential の欠落・偽造・失効は state を変更せず拒否する。

## プロトコルとライフサイクル

対応する MCP protocol version は `2025-06-18` である。クライアントは接続ごとに同じ版を
`initialize.params.protocolVersion` へ指定する。省略や異なる版を送ると server は値を echo せず
`Invalid params` を返す。

接続は次の順で ready になる。`ping` を除く tool/resource request は ready になってから受理する。

```text
awaiting initialize
        |
        | initialize request / initialize response
        v
awaiting initialized
        |
        | notifications/initialized
        v
ready ---- tools/*, resources/*
```

`initialize` の重複、`notifications/initialized` を request として送ること、ready 前の
tool/resource request は `Invalid Request` になる。順序外または重複した通知は応答せず状態も変更しない。
すべての通知は応答を返さず、`tools/call` を通知として送っても tool を実行しない。

JSON-RPC message は top-level object で、`jsonrpc: "2.0"`、string または整数の `id`、string の
`method`、object の `params`（指定時）を持つ。batch（top-level array）は扱わない。routing 前に
envelope を検証するため、不正な通知が store や daemon に effect を起こすことはない。

| 条件 | code | response id |
|---|---:|---|
| JSON として parse できない | `-32700` Parse error | `null` |
| top-level、`jsonrpc`、`id`、`method` が不正 | `-32600` Invalid Request | 有効な request id。id 自体が不正なら `null` |
| 未知 method | `-32601` Method not found | request id |
| `params` または protocol version が不正 | `-32602` Invalid params | request id |
| tool/daemon の実行中エラー | `-32603` Internal error | request id |

`id` の無い object は notification として扱うため、validation error を含めて response は返さない。
不正入力を受けても stdio serve loop は次の行を処理し続ける。

## JSON-RPC メソッド

serve ループが応答するメソッドは次のとおり。1 行 = 1 メッセージで、通知（`id` 無し）には
応答しない。不正入力 1 行ではサーバを止めず、リクエスト単位のエラーは JSON-RPC エラー応答に
整形する。

| メソッド | 役割 |
|---|---|
| `initialize` | 対応プロトコル版、capabilities（`tools` / `resources`）、`serverInfo` を返す |
| `ping` | 空の結果を返す（疎通確認） |
| `tools/list` | 全 tool の `name` / `description` / `inputSchema` を返す |
| `tools/call` | tool 名で実行を dispatch する |
| `resources/list` | 公開 resource の `uri` / `name` / `description` / `mimeType` を返す |
| `resources/read` | `uri` を指定して resource 本文（`contents`）を返す |

## tool 面

tool は系統ごとに分かれ、`tools/list` に載る `name` と `inputSchema` が公開 wire 契約の正本である。
現在のレジストリは 47 件を返す。`tools/list` への掲載は metadata の公開を意味し、durable な実行経路が
あることを意味しない。`tools/call` の実挙動は次のとおりである。

| tool | 実挙動 |
|---|---|
| `session_create` / `session_remove` / `session_recover_legacy` | daemon IPC を通じて session lifecycle store と worktree を操作する |
| `session_list` / `session_status` | daemon の durable lifecycle snapshot を返す。`session_status` は agent phase と worktree の branch/status/dirty/merged も投影する |
| `session_prompt` | `auto` / `queue` / `live` を daemon が解決し、次回 Agent launch 用の durable queue または live Agent PTY へ配送する |
| `session_delegate_issue` | session 作成と durable prompt queue 投入を 1 回の daemon request で完了する |
| `session_delegate_brief` | session を作成し、認証済み caller が一意に選択した worker へ brief を直ちに dispatch する |
| `session_pr` | daemon-owned PR inventory の revision、PR entry、merged 集約を返す |
| `session_complete` | 認証済み session Agent の完了メッセージを workspace root coordinator へ `auto` 配送する |
| `session_note_*` / `session_todo_*` / `session_decision_*` | 認証済み MCP child の session worktree にある machine-local scratchpad を core usecase 経由で読み書きする |
| `user_decision_request` / `user_decision_get` / `user_decision_list` / `user_decision_resolve` / `user_decision_cancel` / `user_decision_expire` | caller credential を daemon 側の live Agent runtime と照合して user-decision store を操作する。request は durable な pending decision を作成し、TUI の resolve 後に `decision_id` と回答を同じ MCP 応答で返す。agent 経路は作成した owner/run の decision だけを操作できる |
| `issue_*` / `memory_*` | cwd の Markdown store を core usecase 経由で操作する |
| `session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox` | caller credential を live Agent runtime と照合し、daemon-owned worker PTY と dispatch store/inbox を操作する |
| `supervisor_start` / `supervisor_get` / `supervisor_list` / `supervisor_cancel` / `supervisor_resolve_escalation` / `supervisor_events` | IPC connection から daemon が導出した caller provenance の範囲で、durable supervisor aggregate を作成・観測・制御する |

agent は durable effect を保証する行だけを実行手順に使う。daemon は handler の無い action の入力
payload を成功応答としてエコーしない。

dispatch 系は credential から caller と current run を復元する。`session_dispatch` は session を作成または再利用し、
その session worktree で worker PTY を起動して run/agent/binding を durable に保存する。worker の
`agent_complete` / `agent_fail` は保存済み binding の caller inbox だけへ配送され、`agent_inbox` は
認証済み caller 自身の inbox だけを返す。payload の caller 名や cwd から identity を補完しない。

`session_delegate_brief` も同じ credential/provenance と worker selector を使う。`agent` は既存 worker の
`id`、または allowlist にある `runtime` と `model` の組のいずれか一方だけであり、混在・部分指定は受理しない。

`supervisor_start` は root task と初期 DAG を snapshot と append-only event journal に保存し、同じ
`idempotency_key` の再送では同じ run を返す。get/list/events の応答は instruction body を含まない安全な
projection である。cancel と escalation resolution は run 作成時に daemon が記録した caller provenance と
一致する IPC connection からだけ受理する。daemon は起動時と Agent completion 時に共有
`SupervisorRuntime` を tick し、dispatch の terminal fact を aggregate へ反映する。

issue / memory の store 系 tool は、CLI 面と同じ `usagi-core` usecase に cwd と実時計を
束縛する薄い adapter である。成功時は usecase の結果 JSON を MCP の text content に入れて
返し、作成・更新・削除は応答前に cwd 配下の source Markdown へ永続化される。派生 index / TOC
の refresh failure は committed source の成功応答を error に変えず、dirty marker により次の
read で自己修復する。commit point と retry の正本は
[2. アーキテクチャ](02-architecture.md#markdown-永続化の-commit-contract)を参照。
`issue_get` / `memory_get` は対象が無ければ `null`、delete は `deleted: boolean` を返す。
検索は query 省略で全件を返し、issue には `ready` / `unmet_deps` を付与する。

issue store は git 追跡対象なので、`issue_create` / `issue_update` / `issue_delete` は
`.usagi/sessions/<name>/` 配下の session worktree からだけ実行できる。workspace root の
コーディネータからの呼び出しは store を変更せず拒否する。memory store の書き込みはこの
制約の対象外である。

TUI の人間回答面は MCP caller credential を持たない。daemon は agent 用 `DispatchTool` と別の型付き IPC
request として workspace-scoped な `get` / `list` / `resolve` / `cancel` だけを受け付け、`request` と
`expire` は credential 付き agent 面に限定する。`resolve` は回答と delivery outbox を atomic に保存してから
`tools/call` の成功応答を返す。consumer は outbox、durable decision の owner・回答、live runtime の operation
fence、dispatch binding を照合し、すべて一致するときだけ同じ run の PTY へ continuation prompt を送って event を
ack する。PTY delivery failure や MCP client disconnect では event を残して再試行し、daemon restart で runtime
identity を復元できない場合は fail-closed で配送しない。期限切れ、cancel、expire は terminal record のみを残し、
回答 notification を作らない。deadline maintenance は接続や次の MCP call を待たずに期限を terminal 化する。

## resource 面

resource は**静的テキスト**（`uri` / `name` / `description` / `mimeType` / `text`）で、agent は
`resources/list` で発見し `resources/read` で本文を取得する。`initialize` の capabilities に
`resources` を宣言する。tool（振る舞い）と分離し、「実行はしないが agent に読ませたい」導線を
配信するのに使う。

resource のレジストリと応答 `Value` の組み立ては純関数（`crates/cli/src/mcp/resources.rs`）に
閉じ、serve ループ側は薄い glue に保つ。本文はクレート同梱の Markdown アセットを埋め込む。

## orchestration ガイド

現在公開している resource は orchestration の利用ガイド 1 つである。

| URI | mimeType | 内容 |
|---|---|---|
| `usagi://guides/orchestration` | `text/markdown` | session lifecycle と dispatch/observe/complete/inbox の手順（agent 向け） |

ガイドは `tools/list` に載る実在の tool 名だけを使い、daemon を権威とする orchestration の
経路と制約を説明する。durable effect の無い tool を手順には含めない。agent 起動プロンプトへ
大きな説明文を注入せず、必要な導線はこの resource で発見させる。
