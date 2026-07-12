# 提案: v2 daemon IPC／ID overview と identity

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [入口面](01-entry-surfaces.md) ｜ 次へ → [IPC protocol](03-ipc-protocol.md)

本書は **v2 の未実装設計**であり、現在仕様の正本ではない。現在のクレート責務と依存方向は
[2. アーキテクチャ](../02-architecture.md)、現在実装されている起動面は
[1. プロジェクト概要](../01-overview.md)を参照する。実装ロードマップは
[Epic #213](../../.usagi/issues/213-feat-ipc-v2-daemon-authoritative-ipc-id.md) とその子 issue で追跡する。

本提案セットが実装前の IPC／ID 設計判断の SSoT である。本書は目標・権威・ID不変条件、topic別の詳細は
[IPC protocol](03-ipc-protocol.md)、[terminal/session API と security](04-daemon-api.md)、
[daemon lifecycle・配置・実装計画](05-daemon-lifecycle.md)を正本とする。実装後は契約を仕様書へ移し、本提案セットをリンクstubにする。

## 目次

- [設計目標](#設計目標)
- [調査結果と継承する判断](#調査結果と継承する判断)
- [権威と resource graph](#権威と-resource-graph)
- [ID 体系と不変条件](#id-体系と不変条件)
- [詳細提案](#詳細提案)

## 設計目標

### 保証すること

- daemon を PTY/process、vt100/scrollback、terminal registry、session runtime 監視、queue/autostart、
  managed session 状態変更の実行権威に一本化する。
- TUI は描画・tab/focus/layout、CLI/MCP は入口 adapter に限定し、TUI を閉じても daemon 所有 terminal と
  accepted operation が継続する。
- remove 後の同名 session 再作成、stale client、late worker、複数 Agent pane、daemon generation rollover、
  response/ACK loss で、別 resource への誤配送・二重 spawn・二重 prompt・誤 kill を起こさない。
- request/response と server push を同じ connection で相関し、順序欠落を cursor で検出して resume または
  full resync できる。
- frame、入力、出力履歴、client queue、in-flight request を有界にし、遅い client が PTY や他 client を止めない。
- managed session/terminal は daemon 不在・不整合・ownership 不明時に local PTY へ暗黙 fallback しない。

### 初期保証に含めないこと

- daemon process 自体が crash した後の画面・入出力継続。PTY master fd は PID や JSON registry から復元できない。
- 複数 host 間の remote IPC、別 Unix user との共有、Windows named pipe。
- 全 stream を横断する global total order。
- 任意 command、argv、secret environment を client が組み立てて daemon に実行させる API。

初期保証は **client/TUI の disconnect 耐性**である。planned restart は旧 daemon process を terminal owner として
draining させることで継続する。daemon crash をまたぐ完全継続は
[PTY broker／FD handoff の将来 issue](../../.usagi/issues/221-docs-daemon-pty-broker-fd-handoff-crash.md)へ分離する。

## 調査結果と継承する判断

### 根拠

| 根拠 | 確認したこと | 本設計への反映 |
|---|---|---|
| [2. アーキテクチャ](../02-architecture.md) | protocol 型と共有ロジックは core、PTY/socket server は daemon、attach client は TUI、各面は core 以外へ相互依存しない | クレート境界と依存方向を維持する |
| [入口面 proposal](01-entry-surfaces.md) | session 系 CLI/MCP は daemon IPC、TUI は daemon push を描画するだけ | daemon を managed state の単一書き手にする |
| [v1 daemon proposal](../../v1/document/proposals/02-daemon.md) | daemon PTY/vt100 権威、attach、TUI disconnect 後の継続 | ownership model を継承する |
| [v1 lifecycle proposal](../../v1/document/proposals/05-session-lifecycle.md) | `SessionLifecycle`、session incarnation、attempt/operation fencing、revision、crash reconcile | 状態機械を v2 の単一書き手モデルへ移す |
| `v1/src/domain/daemon_ipc.rs` | bounded frame/backlog、typed Missing/Adopted、snapshot/output/exited | bounded stream と ownership unknown の区別を継承する |
| `v1/src/usecase/daemon_ipc.rs` | terminal/attach registry と client ごとの output cursor | pure registry/reducer と IO を分離する |
| `v1/src/main.rs` の `DaemonIpcServer` | socket、PTY、session cache、queue、通知、永続化が一つの型へ集中 | connection/router/terminal/session/operation を分割する |
| `v1/src/presentation/tui/home/terminal/pool.rs` | tab に加え PTY backend、監視、queue、prompt、通知、teardown を集中所有 | TUI へ実行権威を戻さない |
| [旧 #208](../../.usagi/issues/208-feat-daemon-durable-start-claim-queued-prompt-consumer-daemon.md) | durable claim と ACK/dedupe の必要性 | operation journal と prompt transaction に取り込む |
| [#209](../../.usagi/issues/209-feat-daemon-live-terminal-generation-rollover-orphan-safety.md) | generation-bound terminal、drain、kill ACK、PID reuse 対策 | v2 rollover issue として再定義する |

v1 proposal は判断の履歴であり、v2 の実装済み仕様ではない。特に v1 の registry は terminal id/worktree path/PID を保存して
生存 PID を「adopt」するが、PTY master fd は復元しない。本設計ではこれを **attach 可能な adopt ではなく orphan 検出**と呼ぶ。

### 現行 v2 との差

現行 v2 の `crates/core/src/infrastructure/ipc/` は `Ping` / `Pong { version }` と u32 BE length-prefix frame、
`crates/daemon/src/presentation/ipc.rs` は一接続を逐次処理する handler までを実装している。typed ID、`TerminalRef`、
`AgentRuntimeRef`、late-worker completion fence、legacy identity の fail-closed migration は
`crates/core/src/domain/id.rs` に実装されている。Unix socket bind/connect、client、handshake negotiation、server push、
terminal/session API は未実装である。

現行 `read_frame` は advertised length の上限検査前に payload を確保し、length prefix の 1〜3 byte 途中 EOF も clean EOF と
区別しない。これは本 proposal の bounded codec issue で置き換える。既存 `Ping/Pong` を「本設計が部分実装済み」とは扱わない。

### v1 から継承する契約と捨てる前提

| 継承する契約 | 捨てる前提 |
|---|---|
| daemon が PTY、vt100、scrollback の権威 | daemon 内だけの単調 `u64` terminal id |
| detach / client disconnect と kill を分ける | path/name/PID を resource identity にすること |
| bounded output backlog、gap 時の snapshot resync | raw command、argv、secret env を `Spawn` で送ること |
| PTY 全 byte write 後の ACK | ACK timeout 後の blind retry（at-least-once input） |
| 最終 Output → Exited の順序 | socket `0600` だけで peer を信頼すること |
| definitive missing と ownership unknown を分ける | PID 生存だけで attach 可能に adopt すること |
| explicit kill 完了を ownership 解放条件にする | TUI-local managed PTY fallback |

## 権威と resource graph

### 面ごとの権威

| 対象 | 権威 | client が持つもの |
|---|---|---|
| session lifecycle / setup / remove | active daemon generation | snapshot projection、選択状態 |
| Agent phase / runtime liveness | daemon の runtime registry（hook report と process event を reducer へ入力） | 表示用 projection |
| branch status | core の git 語彙を daemon が観測・publish | sort/filter/表示 |
| queue / autostart / prompt delivery | active daemon generation | enqueue intent と operation 参照 |
| PTY / process group / vt100 / scrollback | terminal を spawn した daemon generation | parser/view cache、tab/focus |
| terminal resize | terminal owner generation（MVP は最後に受理した resize） | 自 client の希望 geometry |
| issue / memory の git-tracked store | 従来どおり cwd の core usecase | CLI/MCP adapter |

active daemon だけが session/control と新規 terminal spawn を実行する。draining daemon は自 generation が既に所有する
terminal の attach/input/resize/scrollback/kill だけを処理する。

### resource graph

```text
WorkspaceId
├── root WorktreeId
└── SessionId（1 record incarnation。name は属性）
    ├── WorktreeId（repository ごとの checkout incarnation）
    │   ├── TerminalRef { DaemonGeneration, TerminalId, scope }
    │   │   └── AgentRuntimeId?（shell terminal では無し）
    │   └── BranchStatus
    └── SessionLifecycle

OperationId（durable intent）──► progress / final result
ClientId
└── RequestId（one RPC）── response cache ──► immediate result / OperationId参照
```

root terminal は `SessionId = null`、managed session terminal は `SessionId` 必須とする。どちらも `WorkspaceId` と
`WorktreeId` を持つ。複数 repository workspace では一 session に複数 `WorktreeId` があり、各 worktree に複数 terminal と
複数 Agent runtime を持てる。

## ID 体系と不変条件

### ID の定義

ID は wire 上 lowercase canonical UUID string とし、型ごとの Rust newtype で相互代入を防ぐ。発行値は 128 bit で再利用しない。
UUID の timestamp を identity、resource order、liveness の判定に使わない。例外としてproducer-issued `OperationId`はUUIDv7とし、
timestampを**新mutationのadmission expiryだけ**に使う。既存operationの進行・query・回収期限には使わない。

| ID | 発行者 / lifetime | 永続化 | 不変条件 |
|---|---|---|---|
| `WorkspaceId` | workspace 登録時 / 登録 record lifetime | workspace registry | unregister 後の同 path 再登録は新 ID。明示 move は ID を保ち path を再検証 |
| `SessionId` | create 予約時 / session record incarnation | workspace state | name が同じでも remove→create は新 ID。rename/display name と独立 |
| `WorktreeId` | checkout record 作成時 / その物理 checkout incarnation | workspace state | repository/path を再構築したら新 ID。path は属性 |
| `TerminalId` | terminal reservation 時 / terminal tombstone retention まで | generation terminal registry | terminal owner generation 内でも再利用しない |
| `AgentRuntimeId` | Agent process 起動予約時 / 一回の Agent runtime | runtime/operation registry | worktree 単位で共有しない。restart は新 ID |
| `ClientId` | client process 起動時 / process lifetime | client memory | reconnect では再利用可、次 process では新 ID |
| `ConnectionId` | server hello 時 / 一 socket connection | 非永続 | 同じ `ClientId` の並行接続を分け、detach/cleanup の単位にする |
| `RequestId` | client が一 RPC 作成時 / response cache window | bounded response cache | 同じ `ClientId` 内で再利用しない。同じ transport retry だけ再利用可 |
| `OperationId` | durable intent producer が送信/publish 前 / operation retention | operation journal | UUIDv7。一logical durable mutationに一つ。RPC correlation・execution attemptと別軸 |
| `DaemonGeneration` | daemon process 起動時 / generation record lifetime | daemon generation registry | restart ごとに新 ID。PID、build、protocol version と別軸 |
| `ProtocolVersion` | build time / wire contract | code / hello | `generation` は破壊的世代、`revision` は同 generation 内の additive revision |

`BuildIdentity` は ID ではない。package version、commit、target、任意の executable digest を診断用に交換するが、互換可否は
`ProtocolVersion` と capability の積集合だけで決める。同じ build でも daemon generation は異なり、異なる build でも protocol と
required capability が合えば通信できる。

### aggregate reference

effecting terminal command は裸の `TerminalId` を受け取らない。

```text
TerminalRef = {
  daemon_generation,
  terminal_id,
  workspace_id,
  session_id?,
  worktree_id
}

AgentRuntimeRef = {
  agent_runtime_id,
  terminal: TerminalRef,
  session_id
}
```

daemon は aggregate ref の全 field を registry entry と constant-time equality で照合する。一つでも違えば `stale_target` とし、
path から「たぶん同じ terminal」を再探索しない。保存 pane も `TerminalRef` 全体を保存する。

### fencing matrix

| 競合 | 適用に必要な fence | 不一致時 |
|---|---|---|
| 同名 session / rollover 前のlate worker | `WorkspaceId + SessionId + OperationId + owner DaemonGeneration + execution/lifecycle attempt + expected state_revision` | no-op を trace し `stale_target` |
| remove 中の prompt/spawn | `SessionId + lifecycle=available + expected state_revision` を reservation commit まで保持 | queue/spawn を作らず `revision_conflict` |
| old client の terminal input | `TerminalRef` 全 field + owner generation の live registry | write せず `stale_target` / `ownership_unknown` |
| 複数 Agent pane の phase report | `AgentRuntimeRef + source_seq + runtime capability` | 他 runtime を更新せず reject |
| daemon rollover 後の control mutation | hello の target generation = current active generation | `generation_rolled_over` と current locator |
| RPC retry / response cache | `(ClientId, RequestId, wire body hash)` | same key/different bodyは`idempotency_conflict` |
| durable mutation retry | `OperationId + target scope + semantic body digest` | 同operationを返すかconflict。新effectを作らない |
| old output/snapshot | `StreamRef/epoch + stream_sequence + resource revision/output offset` | client reducer が破棄または resync |

name、canonical path、PID は照合の追加 evidence には使えるが fence の代わりにしない。PID へ signal を送る場合は process start identity と
process group/session identity も一致させる。

## 詳細提案

| topic | 正本 |
|---|---|
| frame、handshake、envelope、stream、idempotency、backpressure、error | [IPC protocol](03-ipc-protocol.md) |
| terminal API、session/control API、launch/path security | [daemon API と security](04-daemon-api.md) |
| active/draining rollover、crash orphan、clean architecture、issue・test計画 | [daemon lifecycle](05-daemon-lifecycle.md) |
