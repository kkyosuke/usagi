# 7. MCP サーバ（agent 入口面）

> [ドキュメント目次](README.md) ｜ ← 前へ [6. 開発規約](06-conventions.md) ｜ 次へ → [8. coverage exclusion inventory](08-coverage.md)

`usagi mcp` は AI エージェント向けの入口面で、stdio 上の JSON-RPC 2.0 で tool と resource を
公開する。面の責務・経路・daemon を権威とする反映フローの設計判断は
[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md) が正本で、本章は現在の
ビルドが公開する wire 面をまとめる。

## 目次

- [起動と経路](#起動と経路)
  - [daemon Agent への local LLM 配線](#daemon-agent-への-local-llm-配線)
- [プロトコルとライフサイクル](#プロトコルとライフサイクル)
- [JSON-RPC メソッド](#json-rpc-メソッド)
- [tool 面](#tool-面)
  - [session lifecycle の受理契約](#session-lifecycle-の受理契約)
- [tool descriptor と追加手順](#tool-descriptor-と追加手順)
- [resource 面](#resource-面)
- [orchestration ガイド](#orchestration-ガイド)

## 起動と経路

`usagi mcp` は合成ルートが stdin/stdout を束ねて serve ループを回す（エージェントが spawn する
stdio プロセスで、CLI からは隠している）。起動時に daemon へ接続し、停止中なら autostart する。
daemon に接続できなければ stdio serve ループを開始しない（[2. アーキテクチャ](02-architecture.md)、
[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)）。

合成ルートは完全な process argv の解析に成功してから daemon bootstrap と stdio serve を始める。
MCP 入口の文法・usage error・終了 status は
[2. アーキテクチャの process argv contract](02-architecture.md#process-argv-contract) を正本とする。

daemon-provisioned MCP child は private caller credential を IPC に forward する。dispatch/agent tool と `user_decision_*` は
この credential を持つ live daemon Agent runtime だけが利用でき、手動の `usagi mcp` や credential の無い
MCP caller は `ownership_unknown` で fail-closed となる。caller identity、session 名、cwd、path を
tool payload や環境から補完して認可することはない。

Codex を daemon が起動するときは、注入した `usagi` stdio server だけにこの credential と `USAGI_HOME` を
`env_vars` で forward し、server の tool approval mode を `approve` にして各 MCP call の対話確認を省略する。
認可を省略するものではなく、daemon は credential、live runtime、dispatch
binding を引き続き照合するため、credential の欠落・偽造・失効は state を変更せず拒否する。
daemon-provisioned MCP child には同時に daemon が解決した workspace root を一時的に渡す。session worktree の
cwd から起動した server も、この trusted root にある Workspace 設定を Global 設定へ重ねて issue / memory の
tool availability を解決する。同じ trusted root は daemon 接続時に申告する workspace にもなる
（正本は [4. daemon IPC#workspace fence](04-ipc.md#workspace-fence)）。workspace root は認可上の caller identity
には使用しない。

### daemon Agent への local LLM 配線

本節が daemon-owned Agent launch に optional `usagi-llm` MCP server を配線する条件と順序の正本である。
設定は daemon が Global `settings.json` から読む `local_llm.enabled` / `local_llm.model` だけを権威とし、
client request や IPC payload から model を受け取らない。

| 設定 | Claude / Codex の MCP 配線 | system prompt |
|---|---|---|
| `enabled = false`（既定） | 既存の `usagi` だけ。`usagi-llm` の server 名・command・argv を一切載せない | scope 別 instruction だけ。delegation instruction を合成しない |
| `enabled = true` | `usagi` の直後に `usagi-llm` を追加し、同じ usagi binary を `llm-mcp --model <model>` で起動する | scope 別 instruction の後ろに `local_llm_ask` への delegation instruction を合成する |

`model` は `qwen2.5-coder:7b` / `qwen2.5-coder:3b` / `qwen2.5-coder:1.5b` /
`qwen2.5:7b` の closed allowlist である。Global 設定ファイルを手編集して allowlist 外の値を置いても、
storage load 時に `qwen2.5-coder:7b` へ sanitize してから daemon provisioner へ渡す。Claude は
`serde_json` で MCP config を、Codex は TOML basic string の escape を通した `-c` override を組むため、
command path と model は別 server、別 override、shell command を作れない。

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
1 message の JSON payload 上限は末尾 LF を除いて 1 MiB である。reader は最大 1 MiB + LF しか
buffer に確保せず、上限超過時は request / notification の routing や error response を行わず、stdio
connection を fail-closed で終了する。上限内の不正入力では stdio serve loop は次の行を処理し続ける。

## JSON-RPC メソッド

serve ループが応答するメソッドは次のとおり。1 行 = 1 メッセージで、通知（`id` 無し）には
応答しない。入力上限内の不正な 1 行ではサーバを止めず、リクエスト単位のエラーは JSON-RPC
エラー応答に整形する。

| メソッド | 役割 |
|---|---|
| `initialize` | 対応プロトコル版、capabilities（`tools` / `resources`）、`serverInfo` を返す |
| `ping` | 空の結果を返す（疎通確認） |
| `tools/list` | 実効設定で有効な tool の `name` / `description` / `inputSchema` を返す |
| `tools/call` | tool 名で実行を dispatch する |
| `resources/list` | 公開 resource の `uri` / `name` / `description` / `mimeType` を返す |
| `resources/read` | `uri` を指定して resource 本文（`contents`）を返す |

## tool 面

tool は系統ごとに分かれ、tool descriptor が `name` / description / `inputSchema`、runtime
validator、execution route、caller policy の正本である。Issue / Memory がともに有効な既定レジストリは 49 件を返す。
`tools/list` と `tools/call` は同じ descriptor registry を参照するため、掲載された tool は必ず
1 つの実行経路と caller policy を持つ。`tools/call` の実挙動は次のとおりである。

MCP server は起動時に Global と Workspace の `issue_enabled` / `memory_enabled` を解決し、server lifetime 中は
同じ availability snapshot を使う。`issue_enabled = false` では `issue_*` と `session_delegate_issue`、
`memory_enabled = false` では `memory_*` の adapter を descriptor registry に登録しない。無効な tool は `tools/list` に
現れず、名前を直接 `tools/call` しても `Method not found` となり、store / daemon の effect を起こさない。
session / agent など無効化対象ではない MCP tool は引き続き公開する。
設定変更を反映するには MCP client の再接続または server の再起動が必要である。設定の保存先と継承規則は
[TUI の settings scope](03-tui.md#settings-scope-と-workspace-entry)を正本とする。Global または Workspace の
設定が読み取れない場合は、既定の有効値へ黙って戻さず MCP serve loop の開始前に失敗する。

| tool | 実挙動 |
|---|---|
| `session_create` / `session_recover_legacy` | daemon IPC を通じて session lifecycle store と worktree を操作する |
| `session_remove` | 削除を **受理**して返す。worktree の撤去は daemon の teardown worker が完了させる（[session lifecycle の受理契約](#session-lifecycle-の受理契約)） |
| `session_list` / `session_status` | daemon の durable lifecycle snapshot を返す。`session_status` は agent phase と worktree の branch/status/dirty/merged も投影する |
| `session_prompt` | `auto` / `queue` / `live` を daemon が解決し、次回 Agent launch 用の durable queue または live Agent PTY へ配送する |
| `session_delegate_issue` | session 作成と durable prompt queue 投入を 1 回の daemon request で完了する |
| `session_delegate_brief` | session を作成し、認証済み caller が一意に選択した worker へ brief を直ちに dispatch する。失敗時は作成した session を巻き戻す（[delegation の atomicity](#delegation-の-atomicity)） |
| `session_pr` | daemon-owned PR inventory の revision、PR entry、merged 集約を返す |
| `session_complete` | 認証済み session Agent の完了メッセージを workspace root coordinator へ `auto` 配送する |
| `session_note_*` / `session_todo_*` / `session_decision_*` | 認証済み MCP child の session worktree にある machine-local scratchpad を core usecase 経由で読み書きする |
| `user_decision_request` / `user_decision_get` / `user_decision_list` / `user_decision_resolve` / `user_decision_cancel` / `user_decision_expire` | caller credential を daemon 側の live Agent runtime と照合して user-decision store を操作する。request は durable な pending decision を作成し、TUI の resolve 後に `decision_id` と回答を同じ MCP 応答で返す。agent 経路は作成した owner/run の decision だけを操作できる |
| `issue_*` / `memory_*` | cwd の Markdown store を core usecase 経由で操作する |
| `session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox` | caller credential を live Agent runtime と照合し、daemon-owned worker PTY と dispatch store/inbox を操作する |
| `supervisor_start` / `supervisor_get` / `supervisor_list` / `supervisor_cancel` / `supervisor_resolve_escalation` / `supervisor_events` | IPC connection から daemon が導出した caller provenance の範囲で、durable supervisor aggregate を作成・観測・制御する |

agent は durable effect を保証する行だけを実行手順に使う。daemon は handler の無い action の入力
payload を成功応答としてエコーしない。

### session lifecycle の受理契約

`session_create` / `session_remove` / `session_recover_legacy --apply` の成功応答は
`accepted operation <operation_id> (revision <revision>)` である。この文字列は **operation が受理され durable state に
記録された**ことを意味する。

`session_remove` の受理は「worktree を削除し終えた」ことを意味しない。daemon は session を `deleting` に
遷移させた時点で応答し、worktree の撤去は daemon 所有の teardown worker が続ける
（[5. daemon の session teardown worker](05-daemon.md#session-teardown-worker) が正本）。これにより、数 GB の
`target/` を持つ session の削除でも MCP の 30 秒 deadline 内に応答が返る。agent は完了を次のように観測する。

| 観測 | 意味 |
|---|---|
| `session_list` にその session が `deleting` で残る | teardown が進行中である。remove を再送しても新しい teardown は始まらず、進行中 operation が返る |
| `session_list` からその session が消える | teardown が完了した |
| `session_list` にその session が `failed` で残る | teardown が失敗した。`failure.summary` に原因が入る。名前は保持されるため、その record を `session_remove` すれば同名 session を再作成できる |

daemon を停止・crash させても teardown は失われない。次の daemon 起動時に `deleting` の record から再開される。

dispatch 系は credential から caller と current run を復元する。`session_dispatch` は session を作成または再利用し、
その session worktree で worker PTY を起動して run/agent/binding を durable に保存する。worker の
`agent_complete` / `agent_fail` は保存済み binding の caller inbox だけへ配送され、`agent_inbox` は
認証済み caller 自身の inbox だけを返す。payload の caller 名や cwd から identity を補完しない。

session 作成系は optional role selector を受け取る。`session_create` / `session_delegate_issue` /
`session_delegate_brief` は top-level `role`、`session_dispatch` は `session.role` を使う。daemon が current catalog と
保存済み assignment を検証し、instruction 本文は MCP wire に載せない。catalog・default・conflict の正本は
[10. session role](10-session-roles.md) を参照する。

`session_delegate_brief` も同じ credential/provenance を使うが、`agent` selector は **新規 worker 専用**である。
この tool は dispatch 先の session を自分で作るため、作成前の session に既存 Agent が所属することはなく、既存
worker の `id` branch は schema に現れず daemon も受理しない。`agent` は allowlist にある `runtime` と `model` の
組だけであり、部分指定・混在は受理しない。`session_dispatch` は既存 session を対象とするため `id` branch を持つ。

runtime の closed vocabulary は daemon の profile catalog と共通で、`claude` / `codex` / `sakana-ai` を扱う。
workspace の `.usagi/config.toml` に対応する model allowlist があり、profile の実行コマンドが PATH 上にある runtime
だけが MCP schema に現れる。`sakana-ai` の実行コマンドは `codex-fugu` である。

### delegation の atomicity

`session_delegate_brief` は session 作成と dispatch の 2 つの effect を持つ composite operation だが、成功応答は
その両方が成立したときだけ返る。副作用の無い判定（selector、caller、workspace root の runtime/model allowlist、
runtime 実行ファイル、その operation が既に持つ admission）はすべて worktree 作成の**前**に行う。

作成後に dispatch が失敗した場合の扱いは、spawn の結果が確定しているかで分かれる。

| dispatch の失敗 | daemon の扱い | error の `side_effect` / `details.reconcile` |
|---|---|---|
| 確定した拒否（allowlist、capacity、spawn 失敗など） | 作成した session を durable teardown で巻き戻す（worktree と branch を削除） | `none` / `compensated` |
| 巻き戻し自体を記録できなかった | session はそのまま残る | `partial_or_unknown` / `compensation_failed` |
| spawn の結果が不明（store / journal / reconcile 要求） | session を**残す**。worktree で worker が動いている可能性があるため撤去しない | `partial_or_unknown` / `retained` |

error の `details` には `session_id` と `run_operation_id` も入る。agent はこれを使って
`session_list` / `agent_list` で実際の状態を確認する。

巻き戻しは `session_remove` と同じ durable teardown なので、daemon が途中で停止しても次回起動時に再開される
（[5. daemon の session teardown worker](05-daemon.md#session-teardown-worker)）。`session_remove` と異なり branch も
削除する。delegation は commit を持たない session を作るだけなので branch に成果は無く、branch を残すと同名 session の
再委譲が branch 衝突で失敗するためである。

作成と dispatch の間で daemon が停止した場合は、次回起動が connection を受け付ける前に巻き戻す。delegated create は
durable journal にその由来が記録されており、dispatch store にその operation の run も admission も無い session だけが
対象となる（run があればその operation の結末は dispatch 側が所有する）。

同じ operation id での retry は二重作成しない。create は lifecycle journal から、dispatch は記録済みの結末から
replay される。

`supervisor_start` は root task と初期 DAG を snapshot と append-only event journal に保存し、同じ
`idempotency_key` の再送では同じ run を返す。get/list/events の応答は instruction body を含まない安全な
projection である。cancel と escalation resolution は run 作成時に daemon が記録した caller provenance と
一致する IPC connection からだけ受理する。daemon は起動時と Agent completion 時に共有
`SupervisorRuntime` を tick し、dispatch の terminal fact を aggregate へ反映する。

issue / memory の store 系 tool は、CLI 面と同じ `usagi-core` usecase に cwd と実時計を
束縛する薄い adapter である。成功時は usecase の結果 JSON を MCP の text content に入れて
返し、作成・更新・削除は応答前に cwd 配下の source Markdown へ永続化される。派生 index / TOC
の refresh failure は committed source の成功応答を error に変えず、dirty marker により次の
read で自己修復する。commit point、retry、v1 / v2 共通の issue number 採番 authority の正本は
[2. アーキテクチャ](02-architecture.md#markdown-永続化の-commit-contract)を参照。
`issue_get` / `memory_get` は対象が無ければ `null`、delete は `deleted: boolean` を返す。
検索は query 省略で全件を返し、issue には `ready` / `unmet_deps` を付与する。

issue store は git 追跡対象なので、`issue_create` / `issue_update` / `issue_delete` は
`.usagi/sessions/<name>/` 配下の session worktree からだけ実行できる。workspace root の
コーディネータからの呼び出しは store を変更せず拒否する。memory store の書き込みはこの
制約の対象外である。

同じ issue number の source Markdown が複数ある場合、`issue_get` / `issue_to_prompt` /
`issue_update` / `issue_delete` は衝突した exact path を含む execution error を返し、どの sibling も
選択・変更・削除しない。同一 content の `issue_create` retry も曖昧な既存番号を返さず、
`session_delegate_issue` は typed ambiguity の番号と辞書順の exact path を safe execution error まで保持し、
session registry、worktree、branch、dispatch queue、lifecycle state のいずれも作成・変更しない。
`issue_search` は修復対象を観測できるよう parse 可能な
sibling を exact filename と `ambiguous: true` の別 row として返し、`ready` にはしない。parse 不能な
sibling も衝突判定には含め、重複番号を参照する依存は `unmet_deps` に残す。番号 identity と明示 repair の正本は
[2. アーキテクチャ](02-architecture.md#markdown-永続化の-commit-contract) を参照する。

TUI の人間回答面は MCP caller credential を持たない。daemon は agent 用 `DispatchTool` と別の型付き IPC
request として workspace-scoped な `get` / `list` / `resolve` / `cancel` だけを受け付け、`request` と
`expire` は credential 付き agent 面に限定する。`resolve` は回答と delivery outbox を atomic に保存してから
`tools/call` の成功応答を返す。consumer は outbox、durable decision の owner・回答、live runtime の operation
fence、dispatch binding を照合し、すべて一致するときだけ同じ run の PTY へ continuation prompt を送って event を
ack する。PTY delivery failure や MCP client disconnect では event を残して再試行し、daemon restart で runtime
identity を復元できない場合は fail-closed で配送しない。期限切れ、cancel、expire は terminal record のみを残し、
回答 notification を作らない。deadline maintenance は接続や次の MCP call を待たずに期限を terminal 化する。

## tool descriptor と追加手順

tool descriptor の実装は `crates/cli/src/mcp/tools/` と `crates/cli/src/mcp/tool.rs` に置く。
新しい tool は対象系統の `Tool` 実装へ metadata と schema を定義し、同じ registry entry へ
execution route と caller policy を割り当てる。serve 側へ name match を追加しない。

実効設定で filter された registry は MCP serve loop が入力を受け付ける前に検証される。重複 name、重複 execution route、
route を持つ非公開 entry、明示的に unavailable な advertised capability、不正な object schema は
起動を拒否する。runtime validator は `tools/list` が返した同じ schema（runtime/model の動的列挙を
含む）で arguments を検証してから route を実行する。

tool を追加・変更するときは registry の全件回帰テストへ valid/invalid arguments が自動的に列挙される。
route と caller policy の許可された組、advertised と executable route の全単射も同じテストで検証する。
unimplemented capability を一時的に表現する場合は descriptor の unavailable route と理由を使う。
advertised registry は unavailable entry を拒否するため、実装済み route を割り当てるまで公開されない。

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
