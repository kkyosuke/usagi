# 7. MCP サーバ（agent 入口面）

> [ドキュメント目次](README.md) ｜ ← 前へ [6. 開発規約](06-conventions.md)

`usagi mcp` は AI エージェント向けの入口面で、stdio 上の JSON-RPC 2.0 で tool と resource を
公開する。面の責務・経路・daemon を権威とする反映フローの設計判断は
[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md) が正本で、本章は現在の
ビルドが公開する wire 面をまとめる。

## 目次

- [起動と経路](#起動と経路)
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

## JSON-RPC メソッド

serve ループが応答するメソッドは次のとおり。1 行 = 1 メッセージで、通知（`id` 無し）には
応答しない。不正入力 1 行ではサーバを止めず、リクエスト単位のエラーは JSON-RPC エラー応答に
整形する。

| メソッド | 役割 |
|---|---|
| `initialize` | プロトコル版のエコー、capabilities（`tools` / `resources`）、`serverInfo` を返す |
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
| `session_delegate_issue` / `session_delegate_brief` | session 作成と durable prompt queue 投入を 1 回の daemon request で完了する |
| `session_pr` | daemon-owned PR inventory の revision、PR entry、merged 集約を返す |
| `session_complete` | 認証済み session Agent の完了メッセージを workspace root coordinator へ `auto` 配送する |
| `session_note_*` / `session_todo_*` / `session_decision_*` | 認証済み MCP child の session worktree にある machine-local scratchpad を core usecase 経由で読み書きする |
| `user_decision_request` / `user_decision_get` / `user_decision_list` / `user_decision_resolve` / `user_decision_cancel` / `user_decision_expire` | caller credential を daemon 側の live Agent runtime と照合して user-decision store を操作する。解決後の caller は同じ credential 経路の `user_decision_get` で回答を取得する |
| `issue_*` / `memory_*` | cwd の Markdown store を core usecase 経由で操作する |
| `session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox` | caller credential を live Agent runtime と照合し、daemon-owned worker PTY と dispatch store/inbox を操作する |
| `supervisor_start` / `supervisor_get` / `supervisor_list` / `supervisor_cancel` / `supervisor_resolve_escalation` / `supervisor_events` | IPC connection から daemon が導出した caller provenance の範囲で、durable supervisor aggregate を作成・観測・制御する |

agent は durable effect を保証する行だけを実行手順に使う。daemon は handler の無い action の入力
payload を成功応答としてエコーしない。

dispatch 系は credential から caller と current run を復元する。`session_dispatch` は session を作成または再利用し、
その session worktree で worker PTY を起動して run/agent/binding を durable に保存する。worker の
`agent_complete` / `agent_fail` は保存済み binding の caller inbox だけへ配送され、`agent_inbox` は
認証済み caller 自身の inbox だけを返す。payload の caller 名や cwd から identity を補完しない。

`supervisor_start` は root task と初期 DAG を snapshot と append-only event journal に保存し、同じ
`idempotency_key` の再送では同じ run を返す。get/list/events の応答は instruction body を含まない安全な
projection である。cancel と escalation resolution は run 作成時に daemon が記録した caller provenance と
一致する IPC connection からだけ受理する。daemon は起動時と Agent completion 時に共有
`SupervisorRuntime` を tick し、dispatch の terminal fact を aggregate へ反映する。

issue / memory の store 系 tool は、CLI 面と同じ `usagi-core` usecase に cwd と実時計を
束縛する薄い adapter である。成功時は usecase の結果 JSON を MCP の text content に入れて
返し、作成・更新・削除は応答前に cwd 配下の Markdown store と派生 index へ永続化される。
`issue_get` / `memory_get` は対象が無ければ `null`、delete は `deleted: boolean` を返す。
検索は query 省略で全件を返し、issue には `ready` / `unmet_deps` を付与する。

issue store は git 追跡対象なので、`issue_create` / `issue_update` / `issue_delete` は
`.usagi/sessions/<name>/` 配下の session worktree からだけ実行できる。workspace root の
コーディネータからの呼び出しは store を変更せず拒否する。memory store の書き込みはこの
制約の対象外である。

TUI の人間回答面は MCP caller credential を持たない。daemon は agent 用 `DispatchTool` と別の型付き IPC
request として workspace-scoped な `get` / `list` / `resolve` / `cancel` だけを受け付け、`request` と
`expire` は credential 付き agent 面に限定する。`resolve` は回答と delivery outbox を atomic に保存した後、
consumer が outbox と durable decision の owner・回答を照合して配送済みにする。したがって outbox は未読のまま
滞留せず、元 caller は decision ID を使った `user_decision_get` の再試行で解決回答へ収束する。

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
