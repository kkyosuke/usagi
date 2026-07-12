# 提案: v2 daemon lifecycle／配置／実装計画

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [daemon API](04-daemon-api.md)

本書は **v2 の未実装設計**であり、現在仕様の正本ではない。本書がactive/draining generation、planned restart、
crash orphan、clean architecture配置、段階的issue分割、test strategyについての実装前SSoTである。
ID fenceは [IPC／ID overview](02-ipc-id.md)、transport契約は [IPC protocol](03-ipc-protocol.md)、
terminal/session authorityは [daemon API](04-daemon-api.md)を参照する。

## 目次

- [daemon lifecycle・restart・crash](#daemon-lifecyclerestartcrash)
- [clean architecture 上の配置](#clean-architecture-上の配置)
- [段階的実装](#段階的実装)
- [テスト戦略](#テスト戦略)
- [設計判断と将来案](#設計判断と将来案)

## daemon lifecycle・restart・crash

### generation role

| role | session/control | new terminal spawn | owned terminal IO | 終了条件 |
|---|---|---|---|---|
| `standby` | 不可 | 不可 | 不可 | handoff成功でactive、失敗で終了 |
| `active` | 可 | 可 | 可 | planned rolloverでdraining |
| `draining` | 不可 | 不可 | 可 | owned terminal 0、generation-local terminal operation/output drain後 |
| `retired` | 不可 | 不可 | 不可 | endpoint/record tombstone retention後 |

current locator はactive generationを一つだけ指す。daemon control lockはactiveだけが保持し、session state write、queue claim、autostart、
new spawnはhandshake時だけでなくeffect実行直前とcommit直前にもlock保持とgeneration一致を確認する。draining daemonは
**workspace/session/control stateを書かない**が、自generationのterminal registry、output journal、input dedupe、kill result、tombstoneは
継続して書く。crash後の`orphan_running / identity_unknown / lost`はgeneration serving roleではなく、後述するterminal
ownership/reconcile stateである。

### planned restart

```text
old active                 new daemon                  registry/control lock
    │                           │                              │
    │── spawn standby ─────────►│ bind + hello ready           │
    │ stop new control accepts  │                              │
    │ persist safe checkpoint   │                              │
    │ release control lock ───────────────────────────────────►│
    │                           │ acquire + CAS current/roles ─►│
    │◄──── draining committed ──│                              │
    │ terminal IO only          │ session/control + new spawn  │
    │ zero owned resources      │                              │
    └── exit                    │                              │
```

handoff transition自体をdurable recordにし、new daemonがreadyになる前やlock取得後にcrashしてもreconcilerが「active無し」または
「active一つ」へ収束させる。newがcontrol lockを取得した同じlocked transactionでcurrent locator、old=`draining`、new=`active`をcommitし、
oldとnewが同時にcontrol mutationをcommitできる区間を作らない。

operation journalは`owner_daemon_generation`と`execution_attempt`を持つ。MVP rolloverはrunning external IO中のnonterminal operationが
safe checkpoint/finalになるまで`busy`で拒否する。accepted/queuedでexternal effect前のoperationだけは、old worker停止を確認した後、newが
owner/attemptをCASして再開できる。late old completionはowner/attempt fenceでrejectする。spawn/write/setup等のeffect後でoutcome未commitな
operationは`ambiguous`であり、rolloverを理由にblind再開しない。

保存 `TerminalRef.daemon_generation` はtrusted generation registryでold endpointへrouteする。socket path自体をidentity/refへ保存しない。
active daemonのprompt consumerがold terminalへinputする場合も、owner generationのterminal protocolをclientとして使う。new daemonがold
daemon内部型を直接呼ばない。

new activeはhandoff時にold generationの`TerminalRef` / `AgentRuntimeId`対応を引き継ぎ、owner generationのterminal streamを購読する。
oldの`TerminalExited` / liveness eventはnew activeだけがsession runtime reducerとconcurrency slotへ反映し、draining daemonはworkspace/session
stateを書かない。generation間bridgeが切れた場合は、new activeがtrusted registryからold endpointを再解決して`TerminalList/Reconcile`と
stream cursorで収束させる。old terminal終了をruntime/slotへ反映できないままsilent dropしない。

互換なterminal capabilityがold/new/client間に無ければ、live terminalを持つrolloverは`capability_missing`で拒否する。
build不一致を理由にold terminalをkillしたりlocal spawnへfallbackしたりしない。

### stop policy と generation bound

- plain stop はlive terminalまたはnonterminal operationがあれば `busy` で拒否する。planned restartもunsafe checkpointのnonterminal
  operationがあればbusyだが、live terminalだけならdraining rolloverできる。
- `drain` はnew control/spawnを止めるが既存terminalを継続する。current activeが必要になればautospawnでnew generationを作る。
- `terminate` は各terminal kill operationを開始し、completed ACKとoperation収束後だけdaemonを止める。
- 同時generation数はMVP default 2（active + draining 1）とする。3世代目rolloverはoldest drain完了またはexplicit teardownまで拒否する。
- zero-terminalかつgeneration-local terminal operation/outputをdrain済みのgenerationはidle clientへredirect/closeを通知して自動停止し、
  stale endpoint/recordを冪等回収する。任意長のidle client接続だけで終了を妨げない。

### daemon crash と orphan

daemon crashではPTY master fd、vt100 parser、未永続outputが失われる。子processが生きていても新daemonはscreen/inputへattachできない。

```text
generation process dead
       │
       ├── process identity verified alive ─► orphan_running
       ├── pid reused / evidence不足 ───────► identity_unknown
       └── process gone ────────────────────► lost / tombstone
```

terminal registryはまず`TerminalRef`、`AgentRuntimeId?`、OperationIdのspawn reservationをexternal spawn前にatomic保存し、spawn後に
PID/start identity/process groupを第二のatomic transitionで保存する。その間にcrashしてPID evidenceが無いreservationは
`ambiguous_unidentified`としてreplacementをblockし、「spawnされなかった」と推測して再実行しない。PID aloneの`kill(0)`でownershipを
証明せず、identityを証明できないprocessへsignalを送らない。

- `orphan_running` / `identity_unknown` はattach/inputを拒否し、snapshotに明示する。
- 同じ `SessionId` / `WorktreeId` のAgent orphanが解決するまでautostart/replacement Agent spawnを止める。
- verified terminate、process gone確認、または人による明示acknowledgeをoperationとして記録する。
- registry missing/corrupt、kill ACK lossではownership metadataを先に消さない。
- shellは明示的な別spawn intentを許せるが、stale terminal restoreをfresh shellへ自動置換しない。

完全継続にはmaster fdをdaemon process外で保持するbroker、または生存old processから`SCM_RIGHTS`でhandoffする機構が必要であり、
MVPのorphan reconcileと混ぜない。

## clean architecture 上の配置

### component boundary

```text
Unix accept/connect・signal・PTY fork（合成ルートで実IO注入）
             │
             ▼
daemon presentation: ConnectionSession / Handshake / RequestRouter
             │ command                           │ event/response
             ▼                                   ▼
daemon usecase                         SubscriptionHub / bounded scheduler
├── TerminalSupervisor ── ports ──► PTY / vt100 / process adapter
├── SessionController  ── ports ──► workspace/operation store
├── OperationJournal
├── PromptConsumer
└── GenerationCoordinator ────────► registry/control-lock adapter
             ▲
             │ core domain/reducer/protocol
             ▼
TUI attach projection     CLI command adapter     MCP tool adapter
```

`TerminalPool` や `DaemonIpcServer` という一型へ責務を再集約しない。thread/async taskの数は実装判断だが、state ownershipとportは上図で
分ける。

### 配置表

| 置き場所 | 型・ロジック |
|---|---|
| `crates/core/src/domain/` | typed ID、resource ref、`SessionLifecycle`、`AgentPhase`、`BranchStatus`、`LaunchIntent`、snapshot value |
| `crates/core/src/usecase/` | pure lifecycle/phase reducer、fencing、capability、idempotency decision、resume/resync policy、client port |
| `crates/core/src/infrastructure/ipc/` | wire envelope/type、serde、bounded frame codec、surface-neutral connection state |
| `crates/daemon/src/presentation/` | peer contextを受けたhandshake、request dispatch、typed response/error shaping |
| `crates/daemon/src/usecase/` | terminal/session/operation/prompt/generation authority、registry/reducer orchestration |
| `crates/daemon/src/infrastructure/` | PTY/process/vt100 adapter、generation/operation/terminal store、Unix peer/socket adapter |
| `crates/tui/src/infrastructure/` | attach/subscription client、TerminalSnapshot/Output reducerへのbridge |
| `crates/tui/src/usecase/` | tab/focus/layoutへ渡すview projection、reconnect/resync UX state |
| `crates/tui/src/presentation/` | 描画、key mapping、modal、focus |
| `crates/cli/src/cli/` / `mcp/` | argv/JSON-RPCをcore client requestへ変換し結果を整形 |
| `src/main.rs` | Unix socket/PTY/signal/filesystem実IOの生成とport注入だけ |

[2. アーキテクチャ](../02-architecture.md) と [入口面 proposal](01-entry-surfaces.md) のclient配置の粒度は、
「wire型・surface-neutral client portはcore」「TUI固有attach state machineはTUI」「Unix connect実IOは合成ルート」として具体化する。

## 段階的実装

[Epic #213](../../.usagi/issues/213-feat-ipc-v2-daemon-authoritative-ipc-id.md) の依存順は次のとおり。

```text
#212 design
   │
   ▼
#214 typed IDs
   ├──────────────► #217 lifecycle / durable operation
   ▼
#215 envelope / codec
   ▼
#216 secure transport / backpressure
   ├──────────────► #218 terminal API
   │                     │
   └────► #217 ──────────┴──► #219 session/control + prompt
                                     │
                              #209 rollover/orphan
                                     │
                                     ▼
                              #220 client cutover

future: #221 PTY broker / FD handoff（MVP 非依存）
```

| 段階 | issue | deliverable / cutover 条件 |
|---|---|---|
| 0 | [#214](../../.usagi/issues/214-feat-core-v2-typed-id-fencing-invariant.md) | dormant typed IDとfencing。legacy ambiguityはfail-closed |
| 1 | [#215](../../.usagi/issues/215-feat-core-ipc-envelope-handshake-error-bounded-codec.md) | pure envelope/codec/compatibility/correlation。productionはまだPing/Pong可 |
| 2a | [#216](../../.usagi/issues/216-feat-ipc-secure-unix-transport-bounded-backpressure.md) | secure socket、bounded scheduler、fake/socket test |
| 2b | [#217](../../.usagi/issues/217-feat-core-sessionlifecycle-reducer-durable-operation.md) | dormant lifecycle/operation/migration reducer。consumer未cutover |
| 3 | [#218](../../.usagi/issues/218-feat-daemon-terminal-registry-pty-command-event-api.md) | daemon terminal APIをbehind feature barrierでE2E。TUI local pathはまだproduction |
| 4 | [#219](../../.usagi/issues/219-feat-daemon-session-control-api-durable-prompt-consumer.md) | daemon単独でsession/queue operationを完結。全consumer test |
| 5 | [#209](../../.usagi/issues/209-feat-daemon-live-terminal-generation-rollover-orphan-safety.md) | active/draining handoff、orphan block、restart matrix |
| 6 | [#220](../../.usagi/issues/220-feat-clients-tui-cli-mcp-v2-daemon-ipc-cutover.md) | TUI/CLI/MCPをatomic cutoverし、managed local fallback/direct mutationを除去 |
| 将来 | [#221](../../.usagi/issues/221-docs-daemon-pty-broker-fd-handoff-crash.md) | broker/FD handoffの採否。MVP mergeをblockしない |

段階0〜5のdormant pathは個別にproduction mutationを有効化しない。特にIDだけ付与してname/path consumerを残す、session readerだけ
v2化する、TUIとdaemonを同時writerにする、terminal spawnだけdaemon化してqueueをTUIに残す、といったpartial cutoverを避ける。
段階6でmigration barrier、daemon authority、全client consumer、no-fallbackを同時に有効化する。

## テスト戦略

### test pyramid

| 層 | 対象 | 必須ケース |
|---|---|---|
| pure domain/reducer | ID、fence、lifecycle/phase、dedupe、resume policy、error mapping | same-name recreate、late worker、wrong generation、transition matrix、revision gap、same ID/different body |
| fake IO/store/clock/process | codec、operation journal、bounded queue、cancel/reconcile、rollover | partial prefix/payload、response loss、crash points、slow client、PID reuse、registry corruption |
| real Unix socket | permission/peer、handshake、multiplex、resume/resync、multi-client | mode/uid、hello buffered frames、out-of-order response+push、disconnect、partial write、queue overflow |
| real PTY/process E2E | terminal semanticsとdaemon authority | spawn/output/keys/resize/scrollback/detach/re-attach/kill/final exit、TUI disconnect survival |
| black-box surfaces | TUI/CLI/MCP cutover | TUI無しsession operation、structured error整形、no local fallback、generation routing |

各層で、resource revisionが正当にjumpしてもstream sequenceが連続ならresyncしないこと、同じ`ClientId`のold/new connection overlapで
old disconnectがnew subscriptionを消さないこと、PTYが1..N-1 byte受理後にfatal writeとなること、socket frame途中の`WouldBlock`、
CLI process消滅後のOperationId discovery、running nonterminal operation中のrollover=`busy`、snapshot/output atomic boundary、
hello/attachと同じreadに入ったbuffered snapshotの先drainを回帰testに含める。

### failure matrix

| injection point | 期待結果 |
|---|---|
| operation reservation persist 前 | side effect無し。同processは同RequestId、別processは同OperationIdで初回として受理 |
| operation accepted 後・external IO 前 |同OperationIdから再開可 |
| process spawn 後・registry commit 前 | `ambiguous` / orphan block。自動二重spawn無し |
| PTY write 後・ACK commit 前 | interactive unknown、durable prompt ambiguous。自動二重input無し |
| final Output後・Exited前 | reconcileがEOF/process stateからExitedを一度commit |
| kill signal後・process gone確認前 | ownership保持。retry/reconcileで同kill operation |
| snapshot後・最初delta前 | subscribe barrierにより欠落無し |
| output journal eviction中 | slow clientだけresync。他client/PTY継続 |
| old active quiesce後・new current CAS前 | control writer無しへ収束し、二writerにならない |
| daemon crash + child alive | orphan明示、attach/write/replacement Agent spawn拒否 |
| PID reuse | identity_unknown、誤signal無し |

### 必須 end-to-end scenario

1. 同じsession名をremove→recreateし、旧pane、旧worker completion、旧promptを全て拒否する。
2. 一つのworktreeでAgent paneを二つ起動し、phase、prompt target、exit/killを混同しない。
3. client Aをdetach/disconnect後もprocessが進み、client Bがsnapshot/replayでattachする。
4. slow client Aがbacklogから落ちてもclient Bのechoとcontrol ACKは継続し、Aだけresyncする。
5. input/kill responseをdropして同RequestId/OperationIdで再接続し、二重write/killせず結果を確定する。
6. live terminalを持つplanned restartでoldがdraining、新spawnはnew active、保存paneはoldへrouteする。
7. daemonを強制終了し、childが残る場合はorphanとして表示してAgent replacementをblockする。
8. daemon不在/autospawn失敗時にmanaged TUI/CLI/MCPがlocal PTY/direct state mutationへfallbackしない。

## 設計判断と将来案

| 判断 | 結論 |
|---|---|
| resource key | opaque typed ID + ancestor scope。name/path/PID単独は不採用 |
| daemon互換性 | protocol generation/revision + capability。build equalityは不採用 |
| server push | stream固有cursor + snapshot/resume/resync。global revisionは不採用 |
| mutation retry | bounded RequestId response cache + producer-issued durable OperationId journal。timeoutを失敗扱いする方式は不採用 |
| terminal history | daemon vt100/scrollback権威 + raw output offset。clientごとのfull history複製は不採用 |
| disconnect | detach/subscription cleanupだけ。terminal/operation cancelは不採用 |
| restart | old processをdrainingさせるplanned rollover。live resourceの暗黙killは不採用 |
| crash recovery | explicit orphan/lost。PIDからstreamable terminalへadoptする方式は不採用 |
| launch security | typed intentをdaemon解決。raw command/env wireは不採用 |
| managed fallback | daemon autospawnまたはtyped error。local PTY fallbackは不採用 |

将来brokerを採用する場合も `TerminalRef`、RequestId/OperationId、cursor、typed launch、security boundaryは維持する。変わるのは
`DaemonGeneration`の下にあるPTY ownershipをbroker generationへ委譲する点であり、MVP契約を曖昧にして先回り実装しない。
