# 提案: agent 向け dispatch MCP 契約（他 session の agent への即時委譲と確実な完了報告）

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [PTY crash continuation](07-pty-crash-continuation.md)

`usagi` の agent 向け MCP に、**特定の agent へ指示を出して即時実行させ、完了時は機械的に記録した
呼び出し元 agent へ確実に報告する** dispatch 契約を追加する提案。まだ実装していないため
[06-conventions.md#記載実装済み](../06-conventions.md#記載実装済み) に従い spec 本編（`01-` …）には
書かず、この提案に分離する。実装が確定した部分は [02-architecture.md](../02-architecture.md) /
[04-ipc.md](../04-ipc.md) / [05-daemon.md](../05-daemon.md) へ畳み込み、この提案はリンクだけ残す。

実装タスクは issue ストア（`.usagi/issues/`）の #321–#325 で追跡する（[§8](#8-実装-issue-分割)）。

## 目次

- [1. 目的と背景](#1-目的と背景)
- [2. 現状の棚卸し（再利用する既存プリミティブ）](#2-現状の棚卸し再利用する既存プリミティブ)
- [3. MCP 契約](#3-mcp-契約)
- [4. durable データモデルと置き場所](#4-durable-データモデルと置き場所)
- [5. caller↔worker binding と caller 推論](#5-callerworker-binding-と-caller-推論)
- [6. 完了報告と「報告なし」検知](#6-完了報告と報告なし検知)
- [7. 既存 tool との互換性・移行方針](#7-既存-tool-との互換性移行方針)
- [8. 実装 issue 分割](#8-実装-issue-分割)
- [9. runtime/model allowlist、schema snapshot と再検証](#9-runtimemodel-allowlistschema-snapshot-と再検証)
- [10. 非目標](#10-非目標)

## 1. 目的と背景

現在の orchestration は「起源フロー（root が brief/issue を渡して新 session を起こす）」と
「遂行フロー（session が worktree で作業して PR する）」を [session_delegate_brief](../../.usagi/issues/109-feat-mcp-session-delegate-brief-issue-session.md) /
`session_delegate_issue` で回す（[01-entry-surfaces.md](01-entry-surfaces.md)）。これらは
**session 単位**の粗い委譲で、配送も `queue`（起動時キュー）が前提であり、次の要求を満たさない。

| 要求 | 現状の不足 |
|---|---|
| 他 session の**特定 agent**へ指示する | 委譲先は session 単位。session 内の複数 agent を個別に指名できない |
| **即時実行**させる（queue/live を意識しない） | `session_prompt` は `mode`（auto/queue/live）を公開し、呼び出し側が配送を選ばされる |
| 呼び出し元へ**確実に報告**する | `session_complete` は session→親 session/`:root` へ prompt を送るだけ。宛先を引数で渡すため取り違えうる。親が停止中だと届かない |
| 報告に **PR / commit / 変更ファイル / 検証**を構造化して載せる | 完了メッセージは自由文のみ |
| **完了報告の呼び忘れ**を検知する | 検知経路が無い（呼ばなければ何も起きない） |

本提案はこれを、既存の durable な daemon runtime（reservation → snapshot → spawn → journal → exit）と
typed ID / fence の上に、**agent 単位の dispatch と durable inbox** として設計する。

## 2. 現状の棚卸し（再利用する既存プリミティブ）

新規に作り直さず、既にある durable 機構へ接続する。

| 概念 | 既存プリミティブ | 所在 |
|---|---|---|
| run の同一性 | `OperationId`（UUIDv7・durable operation identity） | `crates/core/src/domain/id/` |
| agent runtime の参照 | `AgentRuntimeRef { agent_runtime_id, terminal, session_id }` | 同上 |
| 遅延完了の照合 | `CompletionFence`（workspace/session/operation/generation/attempt/revision） | 同上 |
| product-neutral な runtime/model | `AgentProfileId`（例: `claude` / `codex`）・`ModelSelector` | `crates/core/src/domain/agent/` |
| 即時 prompt の運搬 | `LaunchRequest.initial_prompt`（launch 時に agent へ渡る） | 同上 |
| session の系譜（親） | `SessionRecord.started_from` / `SessionOrigin::Mcp` | `crates/core/src/domain/session/` |
| daemon 権威の session | `ManagedSession` / `WorkspaceLifecycleState`（`sessions.json`） | `crates/core/src/domain/session_lifecycle.rs` |
| run 実行と永続化 | `RuntimeCoordinator` / `RuntimeStore` / `RuntimeStoreSnapshot` | `crates/daemon/src/usecase/runtime.rs` |
| durable な store 基盤 | `json_file`（atomic write+fsync）/ `store_lock`（cross-process） | `crates/core/src/infrastructure/persistence/` |

**存在しないもの（本提案で新設）**: durable な **inbox**（grep で 0 件）、**agent 単位の永続エンティティ**、
`run_id` を返し caller↔worker を結ぶ **dispatch 契約**。

> daemon が単一書き手である原則（[01-entry-surfaces.md](01-entry-surfaces.md)）を保つため、新設 store は
> daemon state dir 側（`sessions.json` の隣）に置き、typed ID で鍵付けし `CompletionFence` で照合する。

## 3. MCP 契約

公開する tool は次の 7 つ。配送モード（queue/live）は**公開しない**（常に即時実行）。

| tool | 役割 | 主な引数 | 返り値 |
|---|---|---|---|
| `session_dispatch` | session を upsert し、指名 or 新規 agent に prompt を即時実行させる | `session`, `agent`, `prompt` | `run_id`, `session`, `agent_id` |
| `session_get` | ある session の agent 一覧を task 付きで返す | `name` | agents[] |
| `agent_list` | 全 agent を session/status で任意フィルタして返す | `session?`, `status?` | agents[] |
| `agent_get` | 1 agent の run 履歴・結果要約を返す | `agent_id` | agent + runs[] |
| `agent_complete` | 実行中 run の成功を caller inbox へ配送する | `summary`, `result?`, `run_id?` | delivered_to |
| `agent_fail` | 実行中 run の失敗を caller inbox へ配送する | `summary`, `error?`, `run_id?` | delivered_to |
| `agent_inbox` | caller 自身の inbox（他 agent からの報告）を取得する | `since?`, `unread_only?` | messages[] |

### 3.1 `session_dispatch`

```
session_dispatch {
  session: { name: string },                 // upsert: 在れば再利用、無ければ作成
  agent:   { id: string }                     // 既存 agent を利用
         | { runtime: string, model: string },// 新規 agent を runtime+model で作成
  prompt:  string                             // 即時実行させる指示
} -> { run_id: string, session: string, agent_id: string }
```

- `session.name` は **upsert**。存在すれば再利用し、無ければ `session_create` 相当の lifecycle で作成する。
- `agent` は **id 指定**（既存 agent の再利用）**か** `runtime`+`model` 指定（新規作成）の**排他**。
  `id` と `runtime`/`model` の併用は typed error（`ErrorCode::InvalidArgument`）にする。
- `runtime` は `AgentProfileId`（`claude` / `codex` …）、`model` は `ModelSelector`。可否は
  agent capability（#146）で検証する。
- prompt は `LaunchRequest.initial_prompt` に載せ、daemon が**即時 launch**する。queue/live は選ばせない。
- dispatch 成立時に **caller↔worker を durable に binding**（[§5](#5-callerworker-binding-と-caller-推論)）し、
  `run_id`（＝ launch operation の `OperationId`）を返す。

### 3.2 可視化 tool

`session_get(name)` は当該 session の agent を、id / runtime / model / status と
**現在または最後の task**（prompt・開始時刻・状態）付きで返す。

```
session_get { name } -> {
  session, agents: [
    { agent_id, runtime, model, status,
      task: { run_id, prompt, started_at, state } | null }
  ]
}
```

`agent_list` は横断一覧。`session` / `status` で任意フィルタし、各 agent の
id・所属 session・runtime・model・status・task summary・updated_at を返す。

`agent_get(agent_id)` は 1 agent の run 履歴（各 run の prompt・状態・結果要約）を返す。

### 3.3 完了・失敗・受信

`agent_complete` / `agent_fail` は**宛先を引数で受け取らない**。宛先は dispatch 時に保存した
caller から解決する。`run_id` は実行コンテキストから推論できれば省略可（[§5](#5-callerworker-binding-と-caller-推論)）。
`result` は構造化する。

```
agent_complete {
  summary: string,
  result?: {
    pr?: string,                 // 例 "#1234" or URL
    commits?: string[],          // commit SHA
    changed_files?: string[],
    verification?: string        // 実行した gate と結果
  },
  run_id?: string
} -> { delivered_to }
```

`agent_inbox` は caller 自身（親 agent）が受信箱を読む。親が停止中でも次回起動時に取得できる
（inbox は durable。[§6](#6-完了報告と報告なし検知)）。

## 4. durable データモデルと置き場所

daemon state dir（`sessions.json` の隣）に、typed ID で鍵付けした store を新設する。

```
Agent (session 内の指名可能な worker：dispatch をまたいで存続)
  agent_id : AgentId (UUIDv4 incarnation)
  session_id : SessionId
  runtime : AgentProfileId          // claude / codex …
  model : ModelSelector
  status : AgentStatus              // idle / running / exited / failed
  current_run : RunId?              // = OperationId

DispatchRun (1 回の即時実行)
  run_id : OperationId (UUIDv7)
  agent_id : AgentId
  prompt : string
  started_at, ended_at? : DateTime
  status : RunStatus                // running / completed / failed / no_report

DispatchBinding (caller↔worker の durable な結び付き)
  run_id : OperationId
  caller : CallerRef { session_id, agent_id }
  worker : WorkerRef { session_id, agent_id }

InboxMessage (caller の agent 単位 inbox へ配送される報告)
  run_id : OperationId
  from : WorkerRef
  kind : Completed | Failed | NoReport
  summary : string
  result : StructuredResult?        // pr / commits / changed_files / verification
  created_at : DateTime
  read : bool
```

- **Agent** は session をまたぐ dispatch の宛先として存続する（`agent.id` 再利用の受け皿）。
  agent runtime の 1 回の起動（`AgentRuntimeRef`）とは別レイヤで、複数 run を束ねる。
- **inbox は caller の (session, agent) 単位**に durable 保存する（例:
  `<daemon-state>/inbox/<caller_session_id>/<caller_agent_id>.jsonl`。atomic append + `store_lock`）。
  caller プロセスの生死に依存しないので、親が停止中でも次回 `agent_inbox` で取得できる。
- **run_id は `OperationId` を再利用**する（launch operation の同一性）。idempotency journal と
  `CompletionFence` の既存語彙にそのまま乗る。

## 5. caller↔worker binding と caller 推論

`agent_complete` が宛先を引数に取らず、`run_id` を省略できるのは、MCP サーバが**worker の session
worktree 内で子プロセスとして起動する**ため、実行コンテキストから機械的に caller と run を辿れるからである。

```
[caller agent]  session_dispatch(session=S, agent=..., prompt=...)
      │  daemon が worker を launch。DispatchBinding{run_id, caller, worker} を durable 保存
      ▼
[worker agent]  ← その worktree 内で `usagi mcp` が動く
      │  agent_complete(summary, result)         ← 宛先も run_id も渡さない
      ▼
  runner が実行コンテキスト（worker の session/agent）から
  current_run → DispatchBinding → caller を解決
      ▼
[caller の inbox] へ InboxMessage を durable 配送
```

- dispatch 時に caller を**機械的に記録**する（`SessionRecord.started_from` / `SessionOrigin::Mcp` と同じ
  取得点）。呼び出し側は宛先を意識しない。
- worker の current_run が一意に定まれば `run_id` は省略でき、曖昧・不一致は `CompletionFence` で
  no-op に落として取り違えを防ぐ。

## 6. 完了報告と「報告なし」検知

- `agent_complete` / `agent_fail` は同じ inbox 配送経路を通り、`kind` だけが異なる。
- **呼び忘れ検知**: daemon は worker の PTY exit を既に durable に commit する
  （`runtime.exit` / `agent_ipc.exit`）。この exit commit 時に、当該 `run_id` に対応する
  `Completed` / `Failed` の inbox 配送が**まだ無い**場合、runner が `NoReport` の InboxMessage を
  caller へ合成配送する。これにより「終了したのに complete を呼び忘れた」状態も caller に必ず届く。
- late / duplicate / wrong-generation な完了は `CompletionFence` で照合し、二重配送しない。

## 7. 既存 tool との互換性・移行方針

新契約は既存を**置き換えず併存**する。役割で使い分ける。

| 既存 tool | 関係 | 移行方針 |
|---|---|---|
| `session_delegate_brief` | 起源フロー（事前 issue 不要でトリアージ session を起こす） | 維持。dispatch は「特定 agent への即時実行＋報告」で目的が異なる |
| `session_delegate_issue` | 遂行フロー（committed issue を新 session へ委譲） | 維持。基点コミット検証（#110）もそのまま |
| `issue_to_prompt` | issue → prompt 整形 | 維持。dispatch の prompt 生成に組み合わせて使える |
| `session_prompt` | session の agent へ prompt 送信（`mode` 公開） | 維持。dispatch は `mode` を隠蔽した即時実行の上位入口 |
| `session_complete` | session→親 session/`:root` へ自由文報告 | 維持。`agent_complete` は agent 単位＋構造化 result＋durable inbox で粒度が細かい |

- `session_dispatch` は `session_create` + agent 解決 + `initial_prompt` launch + binding を束ねる
  **合成 tool**（`session_delegate_*` と同じ合成パターン）で、新しい実行ロジックを二重に持たない。
- 配送モードの公開は `session_prompt` に閉じる。dispatch は常に即時実行なので queue/live を出さない。
- 実装が確定したら、この互換表を [02-architecture.md#入口面-mcp-の-tool-dispatch](../02-architecture.md) と
  [04-ipc.md](../04-ipc.md) の正本へ畳み込み、本提案はリンクに縮める。

## 8. 実装 issue 分割

層境界と store 境界に沿って 3 段の DAG に分割する。

```
#321 core: dispatch の durable ドメイン + store（基盤）
      │
      ▼
#322 daemon: dispatch を Agent launch runtime へ接続・binding・「報告なし」検知
      │
      ▼
#323 mcp: session_dispatch / *_get / *_list / agent_complete|fail / inbox + caller 推論
      │
      ▼
#324 mcp: runtime/model allowlist schema snapshot
      │
      ▼
#325 daemon: current allowlist / executable の launch 前再検証
```

| issue | スコープ | 依存 |
|---|---|---|
| #321 | Agent / DispatchRun / DispatchBinding / InboxMessage / StructuredResult のドメイン型と durable store（daemon state dir、atomic + lock）。100% ユニットテスト。MCP/daemon 配線はしない | — |
| #322 | `DaemonRequest` に dispatch を追加し、session upsert・agent 解決・`initial_prompt` 即時 launch・run/binding 永続化・run_id 返却・PTY exit 時の「報告なし」合成配送を接続 | #321 |
| #323 | 7 tool を daemon IPC client として実装し、caller/run をコンテキスト推論。互換・移行を正本 docs へ反映 | #321, #322 |
| #324 | workspace runtime/model allowlist、injectable executable locator、MCP schema snapshot、`agent_cli` の段階的移行 | #323 |
| #325 | dispatch launch 前の current allowlist / executable 再検証と safe error | #322, #324 |

## 9. runtime/model allowlist、schema snapshot と再検証

`session_dispatch` の新規 agent branch は、workspace 設定の runtime ごとの model allowlist を正本とする。

```toml
[agents.claude]
models = ["sonnet", "opus"]

[agents.codex]
models = ["gpt-5-codex", "o4-mini"]
```

`runtime` は `claude` と `codex` の closed vocabulary とする。各 `models` はその runtime だけで許可する文字列の集合であり、section 不在・空 allowlist・空文字・制御文字・重複を含む値は当該 runtime を選択不能にする。global UI settings、code-defined adapter catalog、CLI が返すアカウントの model list は allowlist の正本ではない。provider API と Claude/Codex CLI の非対話 model listing は、設定を暗黙に拡張するため使用しない。

MCP server は起動時に workspace 設定と PATH executable locator を一度だけ読み、`RuntimeModelSnapshot` を作る。production locator は `claude` / `codex` を PATH 上で探索し、test は `ExecutableLocator` port へ fake を注入する。allowlist が非空で対応 CLI が存在する runtime だけを schema に載せる。`session_dispatch.agent` は次の排他的 branch を JSON Schema `oneOf` で表す。

| branch | 入力 | 制約 |
|---|---|---|
| existing agent | `{ id }` | `runtime` / `model` を含めない |
| new Claude agent | `{ runtime: "claude", model: enum }` | Claude allowlist と executable が必要 |
| new Codex agent | `{ runtime: "codex", model: enum }` | Codex allowlist と executable が必要 |

schema を迂回する caller に備え、MCP parser と daemon も id/runtime/model の排他性、runtime/model の完全組、allowlist membership を検証する。既存 `session_create` / `session_delegate_issue` / `session_delegate_brief` の `agent_cli` は破壊的変更を避けるため deprecated alias として移行期間だけ受けるが、`runtime` または `agent.id` と混在すれば migration error にする。

schema は server lifetime の snapshot である。CLI install/uninstall、PATH、workspace allowlist の変更後は MCP server の再起動または client 再接続でのみ選択肢を再生成する。listing ごとに schema を変動させない。一方 daemon は dispatch launch の直前に current workspace allowlist と executable availability を再検証する。schema 発行後に設定が狭まる、CLI が削除される、PATH が変わる場合は spawn 前に safe invalid-argument / unavailable として拒否する。

| 層 | deterministic test |
|---|---|
| schema builder | fake locator と in-memory config で runtime ごとの enum、CLI 不在、空 allowlist を検証 |
| MCP parser | `oneOf` 排他性、legacy alias、migration error、unknown model を検証 |
| snapshot lifecycle | listing 後の fake config / locator 変更は不変、server 再生成時のみ更新を検証 |
| daemon dispatch | temporary workspace と fixture executable で current state の再検証、spawn 前拒否、identity scope を検証 |

MCP は runtime/model 以外の path、argv、environment、credential、CLI raw output を daemon に渡さない。これらと provider model list は wire response、log、durable record に保存しない。

## 10. 非目標

- queue/live 配送モードの再設計（`session_prompt` の既存挙動は変えない）。
- `claude` / `codex` 以外の runtime adapter 追加や model allowlist の UI 化（#146 の語彙に従う）。
- daemon crash 後の PTY FD 継続（[07-pty-crash-continuation.md](07-pty-crash-continuation.md) の範疇）。
- TUI からの dispatch 表示／操作 UX（別 issue）。
