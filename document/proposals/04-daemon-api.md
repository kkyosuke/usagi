# 提案: v2 daemon terminal／session API と security

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [IPC protocol](03-ipc-protocol.md) ｜ 次へ → [daemon lifecycle](05-daemon-lifecycle.md)

本書は terminal command/event、session/control、phase/prompt、socket/peer/workspace/launch security の
設計提案である。下記の runtime contract 節だけは実装済みの現在仕様を記録し、残る節は実装前の選択肢として扱う。ID scopeは
[IPC／ID overview](02-ipc-id.md)、共通envelopeとoperation契約は [IPC protocol](03-ipc-protocol.md)を参照する。

## 目次

- [terminal API](#terminal-api)
- [実装済み runtime contract](#実装済み-runtime-contract)
- [session/control API](#sessioncontrol-api)
- [security boundary](#security-boundary)

## 実装済み runtime contract

`usagi-daemon` の `RuntimeCoordinator` は、`LaunchRequest`、`AgentRuntimeRef`、`CompletionFence` を受け、
resolver を一度だけ呼ぶ。resolver が返す `DurableLaunchSnapshot` は runtime reservation と一緒に
`RuntimeStore` へ保存され、その成功後だけ injected `PtySpawner` を呼ぶ。snapshot の request/profile
provenance が入力と一致しない場合は typed error で拒否する。

| 状態 | spawn / slot | reconcile |
| --- | --- | --- |
| `reserved` | durable 保存済み。spawn 前から slot を占有する | spawn 可能 |
| `running` | process identity 保存済み。terminal output を journal する | verified exit を待つ |
| `reconcile_required` | replacement と slot 解放を禁止する | ambiguous spawn、永続化失敗、identity unknown、orphan を保持する |
| `exited` / `reclaimed` | slot を解放する | exit は final output commit 後、reclaimed は `Gone` の確認後だけ到達する |
| `spawn_failed` | definite no-child のみ | retry policy は上位 operation が決める |

output は `OutputJournal` に先に append し、それから `TerminalRegistry` の raw-output replay に入る。
terminal attach、detach、replay、cursor fence は既存 registry の `TerminalRef` contract を使い、disconnect は
PTY を止めない。IPC command/schema、product adapter、secret injection はこの実装の対象外である。

## terminal API

### command / response

| command | 主な入力 | 成功結果 / 契約 |
|---|---|---|
| `TerminalSpawn` | `WorkspaceId`、`WorktreeId`、`SessionId?`、`OperationId`、typed `LaunchIntent`、geometry、scrollback limit | operation。reservation 後に `TerminalRef`、`AgentRuntimeId?`、実効 geometry |
| `TerminalList` | `WorkspaceId`、`SessionId?` filter | terminal metadata。raw env/command は返さない |
| `TerminalAttach` | `TerminalRef`、stream epoch/last sequence、snapshot output offset? | `SubscriptionId` + atomic `TerminalSnapshot` または retained output replay |
| `TerminalDetach` | `TerminalRef`、`SubscriptionId` | この connection のexact attachmentだけ解除。PTY は生存 |
| `TerminalKeys` | attached `SubscriptionId`、`TerminalRef`、`input_seq`、base64 bytes、delivery class | enqueueはall-or-none。write結果はfull / partial-ambiguous / failed |
| `TerminalResize` | `TerminalRef`、cols/rows | clamp 後 geometry と terminal revision。MVP は last accepted wins |
| `TerminalScrollback` | `TerminalRef`、offset/lines | bounded viewport snapshot。live stream cursor は変更しない |
| `TerminalKill` | `TerminalRef`、`OperationId`、reason、expected revision? | operation。process group消滅とfinal output drain後にcompleted |
| `TerminalReconcile` | `TerminalRef` | active/exited/orphan/missing/identity_unknown の観測。spawnしない |

`LaunchIntent` は次のような closed enum とし、daemon が settings と allowlist から executable、argv、cwd、env を解決する。

```text
LaunchIntent
├── Agent { profile_id, model_override?, prompt_operation_id? }
├── Shell { profile_id }
└── Recovery { operation_id, profile_id }
```

client が program、shell command、arbitrary argv、environment map、cwd path を送る field は設けない。

### output / exit stream

```text
TerminalSnapshot {
  terminal_ref,
  terminal_revision,
  output_offset,
  geometry,
  screen_replay_base64,
  mode,
  process_state
}

TerminalOutput {
  terminal_ref,
  start_offset,
  end_offset,
  data_base64
}

TerminalExited {
  terminal_ref,
  terminal_revision,
  final_output_offset,
  exit_status
}
```

daemon の vt100 parser と scrollback が正本である。live client は snapshot の replay bytes を bounded parser に入れ、その後の raw output を
offset 順に適用する。scrollback response は別 viewport であり live cursor を進めない。scrollback 表示中に live output を保持しきれない
client は、live へ戻るとき新しい `TerminalSnapshot` を要求する。

`screen_replay_base64`が復元すると保証する状態はcurrent/alternate screen、cursor position/visibility/style、saved cursor、SGR、origin/wrap、
bracketed paste、mouse/input modeとする。parserが未対応のescape sequenceまで「完全再現」とは呼ばず、対応matrixとgolden PTY testをprotocol
capabilityに結び付ける。

attach登録、vt100 replay bytes、`output_offset`、stream `base_sequence` は同じterminal actor turnで捕捉し、initial snapshotと最初の
Outputの間に欠落を作らない。initial snapshotはattach成功responseに含めるか、同じstreamのbase sequence付き最初eventとして送る。
client readerはsocket readを待つ前にdecoder内のbuffered frameを全てdrainし、hello/attachと同じreadに入ったsnapshotを取りこぼさない。
clientが保存cursorに対応するparser snapshot/stream epochを証明できない場合はoutput replayを要求せず、必ずfresh snapshotから始める。

process exit 時は PTY EOF まで読み、final `TerminalOutput` を journal へ appendし、次に `TerminalExited` を commitする。
kill operation completion は `final_output_offset` と terminal revision を結果に持ち、これより前に ownership record を消さない。
scrollback responseはdecoded 256 KiB以内にclampし、要求範囲が残る場合はcontinuation offsetを返す。current viewport/geometryも上限内に
clampし、single terminal snapshotがframe hard ceilingを超えないようにする。exit後のlate attach/reconcile用にfinal snapshot、exit result、
TerminalRef tombstoneをbounded retentionする。

### input ACK と dedupe

ordinary interactive input と durable prompt delivery を分ける。

| delivery class | key / fence | retry |
|---|---|---|
| `interactive` | `(ClientId, TerminalRef, input_seq, RequestId)` | client は timeout を自動 retryしない。同 seq retryはcached ACK、window外は `idempotency_expired` |
| `operation` | `OperationId + TerminalRef + input step` | durable journal。response loss 後も同 operation で reconcileし、二重writeしない |

public `TerminalKeys` のinteractive入力は、その`ConnectionId`が所有するactive `SubscriptionId`を必須とする。daemonのdurable prompt
consumerはclient attachmentを偽装せず、owner generationのgeneration-authenticated internal terminal portへ`OperationId`付きinput stepを渡す。

`input_seq` は `(ClientId, TerminalRef)` ごとに単調増加し、daemon は high-water と直近 bounded window の body hash/result を terminal lifetime 中
保持する。新規inputはexpected next sequenceだけを受理し、lower seqはcached/expired、higher seqは`sequence_gap`としてwriteしない。
このため並行connectionでもbatch orderを一つにし、ledgerを無制限に保持せず重複writeを防げる。

input queueへのenqueueはcapacityを先に予約してall-or-noneにする。per-terminal actorはbatchの`written_offset`を保持し、`EINTR` /
`WouldBlock`では残りから継続して別batchをinterleaveしない。ACKはqueue enqueueやAgent処理完了ではなく、PTY master kernel endpointへ
全bytesをwriteした後にcommitする。0 byte超をwriteした後にfatal errorとなった場合は、accepted prefix byte count付き`ambiguous`として
全量retryを拒否する。0 byteならdefinitive failureにできる。write後ACK commit前のcrashは
interactive では unknown、durable prompt では operation `ambiguous` とし、同 session Agent を自動再spawn/再injectしない。

### multiple clients

- attach 数は terminal lifetime と無関係で、0 client でも PTY を継続する。
- authorized client は全て input できる。各 input batch は daemon の terminal input queue で直列化され、batch 内 bytes を混ぜない。
- resize は最後に受理した request を採用し、geometry revisionを全attacherへpushする。clientはfocus/host resize時だけ送り、毎frame送らない。
- kill は「この client が開いたか」ではなく workspace/session capability で認可する。成功時は全attacherへ exit を配信する。

## session/control API

### 状態軸を分ける

| 型 | 答える問い | 例 |
|---|---|---|
| `SessionLifecycle` | session 実体が構築・利用・削除・回復のどこか | `creating`, `initializing`, `available`, `deleting`, `failed` |
| `AgentPhase` | 一つの Agent runtime が現在何をしているか | `ready`, `running`, `waiting`, `ended`, `exited` |
| `BranchStatus` | 一つの worktree と統合 branch の Git 関係 | `new`, `dirty`, `local`, `pushed`, `synced` |

`active` や `ready` へ三軸を畳み込まない。表示 adapter は明示 projection を作れるが、snapshot は原値を別 field で返す。

```text
SessionSnapshot
├── identity { WorkspaceId, SessionId, name }
├── lifecycle { state, attempt, OperationId?, changed_at }
├── state_revision
├── worktrees[] { WorktreeId, BranchStatus, ... }
└── runtimes[] {
      AgentRuntimeId,
      TerminalRef,
      AgentPhase,
      phase_revision
    }
```

同じ worktree の複数 Agent pane は `runtimes[]` の別要素である。worktree-scoped phase file 一つで代表させない。

### command surface

| command | 契約 |
|---|---|
| `WorkspaceSnapshot` | `WorkspaceId` の full state と `state_revision` |
| `WorkspaceSubscribe` | cursor から replay、または full snapshot resync |
| `AgentPhaseReport` | `AgentRuntimeRef`、runtime token、`source_seq`、phase。higher accepted seq だけ reducer へ適用 |
| `SessionCreate` | `OperationId`、`WorkspaceId`、name、source `SessionId?`、typed policy。name予約からoperation化 |
| `SessionRemove` | `OperationId`、`SessionId`、expected revision、typed force policy。immutable delete planを保存 |
| `SessionSetupRetry/Continue` | `OperationId`、failed initialize operation の明示回復。保存済みplanを対象 |
| `PromptDeliver` | `OperationId`、`SessionId`、target runtime/policy、prompt、typed launch preference。durable operation |
| `OperationGet/Subscribe/Cancel/Reconcile` | [operation API](03-ipc-protocol.md#operation-api) と共通 |

create/remove/setup の state machine、immutable setup/delete plan、attempt、crash point は
[v1 lifecycle proposal](../../v1/document/proposals/05-session-lifecycle.md) を土台にする。ただし v2 では daemon が唯一の writer であり、
TUI/CLI/MCP process が workspace state lock を取り mutation しない。lock/CAS は daemon generation handoff、migration、atomic persistence に
残す。

`SessionRemove`のdelete planは対象`TerminalRef`をsnapshotし、それぞれのkill operationがcompletedになるまでsession/pane/ownership recordを
除去しない。killがunknown/orphanなら`failed(delete)`へ収束して`SessionId`とname予約を保持し、同名fresh sessionを作ってAgentを複製しない。

### phase ingestion

daemon は Agent spawn 時に `AgentRuntimeId` と一回の runtime capability token を子 process の hook 環境へ注入する。hook adapter は
phase token、runtime ref、単調 `source_seq` だけを daemon へ報告する。token は runtime/generation/sessionにbindし、log/state snapshotに
出さない。

- 別 runtime、別 session incarnation、別 terminal generation の report は rejectする。
- 同じ/lower `source_seq` はduplicate/staleとしてno-op。
- terminal exit/process observationはdaemon内部sourceとしてreducerへ入り、`exited` 後の同runtimeを`running`へ戻さない。
- Agent restartは新 `AgentRuntimeId`。phase resetで旧runtimeを再利用しない。

`PromptDeliver`が`AgentRuntimeId`を省略できるのは、daemon側のprimary-runtime policyで対象が一意に決まる場合だけとする。
eligible runtimeが複数でprimaryを決定できなければ`ambiguous_target`として配送せず、session名やworktree pathで先頭paneを選ばない。

### prompt delivery と autostart

```text
queued
  │ claim + concurrency reservation
  ▼
claimed ──► terminal_reserved ──► input_acknowledged ──► running
   │               │                      │
   └───────────────┴──────────────────────┴──► retry_wait / dead_letter / ambiguous
```

- claim は `OperationId`、prompt本文、target `SessionId`、launch profile、queue generationをimmutable snapshotにする。
- existing Agent delivery とfresh Agent spawnを分ける。terminal-only paneはAgent slot/targetではない。
- concurrency slotはspawn予約時からphase handoff/exitまで数え、phase観測前の二重dispatchを防ぐ。
- promptをqueueから消すのはinput ACK commit後。spawn/input failureは同operationのretry policyへ戻す。
- response/ACK loss、daemon crash、ownership unknownではpromptとterminal claimを保持し、blind spawn/writeしない。
- `SessionLifecycle != available` では通常delivery/spawnをcommitしない。removeは同じsession operation boundaryで新claimを遮断する。
- retry/backoff/dead-letterは回数と時刻をbounded stateとして保持し、後からappendされたpromptへ古いattemptを継承しない。

managed queue consumer はdaemonだけで動き、TUIのsync/idle tickを実行条件にしない。daemon不在時はpromptをqueueに残す。

## security boundary

### socket と peer

```text
<data-dir>/daemon/                         mode 0700, owner = effective uid
├── current.json                          atomic locator
└── generations/<DaemonGeneration>/
    ├── record.json
    └── sock                              mode 0600, socket owner検証
```

- bind 前に parent directory を `lstat` し、owner、mode、symlink 非該当を検証する。
- stale socket は対応 generation record と process identity が definitive dead の場合だけ除去する。
- accept 後、JSON decode 前に Linux `SO_PEERCRED` / macOS `getpeereid` 相当で peer effective uid を取得し、daemon uid と一致させる。
- peer credential を取得できない platform/connection は fail-closed。socket modeだけをauthenticationにしない。
- socket path/record は atomic write/renameし、clientはrecord owner/mode/generationを検証してからconnectする。

### workspace と path

wire の effecting command は `WorkspaceId` / `SessionId` / `WorktreeId` を送り、daemon が永続 registry から canonical path を解決する。

1. workspace registration record の canonical root と allowlist root を読む。
2. target worktree record がその workspace/session incarnation に属することを ID で照合する。
3. target directoryを`O_DIRECTORY`/no-follow相当で開き、directory handleのowner/device/inodeをrecordとallowlistへ照合してeffect中保持する。
4. handle相対の`openat`/`fstatat`/`fchdir`相当を使えるadapterはpath再解決を避ける。使えないadapterはeffect直前・直後にidentityを再検証し、
   変化時は成功をcommitしない。
5. spawn/delete直前にもpathを再canonicalizeし、symlink escape、root差し替え、recordとの不一致を拒否する。
6. delete planは開始時snapshotのID/path/identityだけを対象にし、同名pathを再探索して対象を増やさない。

clientが送ったpathをcanonicalizeして「登録済みに見えるから」実行する経路は作らない。pathは診断表示とexplicit workspace registrationだけに
使う。

### launch と secret

- client は `LaunchIntent` と公開 selector だけを送る。
- daemon はtrusted settings、agent adapter、binary allowlistからprogram/argv/cwd/envを解決する。
- secret envはdaemon process/config/keychain側で注入し、response/snapshot/logへ返さない。
- setup commandはworkspace settingsからoperation開始時にimmutable planへsnapshotし、IPC requestにraw commandを載せない。
- promptは実行commandではないがsensitive inputとして扱い、通常logはOperationId、byte count、hashだけにする。
- arbitrary shell recoveryはexplicit `Recovery` intentと許可済みprofileに限定し、通常Agent/Shell APIへcommand escape hatchを置かない。

MVPのsecurity principalは同一effective uidであり、`WorkspaceId`はsecret capabilityではない。同一uid process同士をworkspace別ACLで分離する
保証はしない。一方、requestごとにregistered workspace allowlist、resource scope、runtime token、lifecycle capabilityを検証し、未登録pathや
別incarnationへのeffectを防ぐ。将来別uid/remote clientを許す場合はworkspace capability/ACLを別protocol capabilityとして追加する。
