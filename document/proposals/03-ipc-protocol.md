# 提案: v2 IPC envelope／transport protocol

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [IPC／ID overview](02-ipc-id.md) ｜ 次へ → [daemon API](04-daemon-api.md)

本書は **v2 の未実装設計**であり、現在仕様の正本ではない。本書がframe、handshake、request/response/event envelope、
stream ordering、resume/resync、idempotency、bounded backpressure、disconnect/error契約についての実装前SSoTである。
resource identityとfencingは [IPC／ID overview](02-ipc-id.md)を参照する。

## 目次

- [IPC envelope と handshake](#ipc-envelope-と-handshake)
- [ordering・revision・resume](#orderingrevisionresume)
- [idempotency と long operation](#idempotency-と-long-operation)
- [bounded resource と backpressure](#bounded-resource-と-backpressure)
- [disconnect・timeout・error taxonomy](#disconnecttimeouterror-taxonomy)

## IPC envelope と handshake

### framing

transport は Unix byte stream、frame は `u32` big-endian payload length + UTF-8 JSON payload とする。length は header を読んだ直後、
payload allocation 前に negotiated `max_frame_bytes` と hard ceiling の両方で検証する。prefix 途中 EOF、payload 途中 EOF、invalid UTF-8、
invalid JSON は clean close ではなく protocol error である。

binary terminal data は JSON number array ではなく base64 string とし、`encoding: "base64"` を明示する。将来 binary frame capability を
追加できるが、同じ protocol generation の required capability なしで表現を切り替えない。

logical snapshotがsingle frameに収まらない場合は、connection-localなtransfer ID、base stream sequence、resource revision、
chunk index/count、total bytes、digestを持つ`SnapshotBegin / SnapshotChunk / SnapshotEnd`へ分割する。一connectionの同時transferは一つ、
logical snapshotは16 MiBをhard ceilingとし、clientは全chunkとdigestを確認してからatomicに適用する。途中disconnect/overflowではstagingを
破棄してresyncし、不完全snapshotを表示stateへmergeしない。

### bootstrap handshake

最初の client frame だけは target daemon generation を nullable にできる bootstrap envelope とする。current locator から接続する client は
expected generation、保存`TerminalRef.daemon_generation`をtrusted generation registryで解決したendpointへ接続するclientはowner
generationを送る。socket path自体はrefに含めない。

```json
{
  "kind": "client_hello",
  "client_id": "019b...",
  "connection_nonce": "019b...",
  "expected_daemon_generation": "019b...",
  "supported_protocols": [
    { "generation": 1, "min_revision": 0, "max_revision": 2 }
  ],
  "capabilities": ["terminal.output.v1", "subscription.resume.v1"],
  "required_capabilities": ["request.correlation.v1"],
  "build": { "version": "0.1.0", "commit": "...", "target": "..." }
}
```

server は handshake deadline 内に一つの `server_hello` または handshake error を返す。

```json
{
  "kind": "server_hello",
  "connection_nonce": "019b...",
  "connection_id": "019b...",
  "daemon_generation": "019b...",
  "generation_role": "active",
  "protocol": { "generation": 1, "revision": 1 },
  "capabilities": ["request.correlation.v1", "terminal.output.v1"],
  "build": { "version": "0.1.0", "commit": "...", "target": "..." },
  "limits": {
    "max_frame_bytes": 1048576,
    "max_in_flight_requests": 128,
    "max_input_batch_bytes": 65536,
    "response_cache_window_ms": 86400000,
    "operation_admission_window_ms": 86400000,
    "max_future_skew_ms": 300000
  }
}
```

- generationごとのrevision rangeに積集合が無ければ `protocol_mismatch`。一つのmin/maxで複数generationのgapを暗黙対応扱いしない。
- required capability が選択結果に無ければ `capability_missing`。
- expected daemon generation が違えば通常 message を受けず `generation_mismatch`。
- draining generation は terminal-owner capability だけを advertise し、session/control capability を出さない。
- hello 中に同じ read で後続 frame が届いても decoder buffer を捨てず、hello 完了後に順番どおり処理する。

### post-handshake envelope

```text
Envelope
├── protocol { generation, revision }
├── daemon_generation
└── kind
    ├── request  { request_id, timeout_ms?, body }
    ├── response { request_id, outcome: ok | accepted | error, body }
    └── event    { subscription_id, stream_ref, stream_sequence, body }
```

request は一つの `RequestId` を持ち、server は受理した request ごとに最終的に一つのresponse valueを生成する。connection lossでdelivery
できない場合もbounded response cacheまたはoperation journalから再取得できる。frameをrequestとしてdecodeできないfatal protocol errorは
response correlation無しでconnectionを閉じてよい。durable mutation のbodyは
producerが事前発行した`OperationId`を必須で持ち、`accepted` responseは同IDと最初のoperation revisionを返す。operationの最終結果は
operation query/streamで観測する。queryやinteractive inputのようなdurable mutationでないRPCには`OperationId`を付けない。

event に `RequestId` は付けない。`ConnectionId` と `SubscriptionId` は一接続内の routing token で、resource identity や再接続 token に
しない。同じ `ClientId` が並行接続しても attachment/subscription は `ConnectionId` ごとに所有し、一方の disconnect で他方をdetachしない。
unknown optional field は同 revision の additive compatibility として無視できる。unknown message type、unknown required enum value、
unknown protocol generation は fail-closed にする。

`StreamRef { stream_id, epoch }`はtopic/resource scopeとjournal incarnationを識別する。epochはowner daemon generationを含み、generationを
またいでcursorを再利用しない。`SubscriptionId`を変えて再接続しても同epochのretained journalならresumeできる。

## ordering・revision・resume

### order の単位

global revision は作らず、意味の異なる順序軸を分ける。

| stream / value | cursor | 保証 |
|---|---|---|
| subscription delivery | `stream_sequence: u64` per `StreamRef` | resource change/delta/eventを共有journalへcommitするごとに単調増加 |
| workspace/session snapshot | `state_revision: u64` per `WorkspaceId` | daemon の durable commit ごとに単調増加 |
| terminal live output | `output_offset: u64` per `TerminalRef` | PTY raw byte の累積 offset。再利用しない |
| terminal metadata / geometry / exit | `terminal_revision: u64` per `TerminalRef` | registry transition ごとに単調増加 |
| Agent phase | `phase_revision: u64` per `AgentRuntimeId` | accepted report/process event ごとに単調増加 |
| long operation | `operation_revision: u64` per `OperationId` | progress/state transition ごとに単調増加 |
| protocol | `ProtocolVersion` | resource event order と無関係 |
| daemon ownership | `DaemonGeneration` | resource revision と無関係 |

`stream_sequence` は配信の欠落・重複検出、resource revision/output offsetはresource reducerのstaleness判定に使い、相互に代用しない。
`StreamRef`はtopic、resource ID、owner daemon generation/journal epochから導出し、`SubscriptionId`を含めない。一 connection の frame bytes は
FIFO だが、異なる request の response completion 順は request 順と一致しなくてよい。cross-stream のevent orderに意味を持たせない。
関連値を atomic に見せる必要がある場合は、一つの snapshot に同じ resource revision で含める。

### subscribe barrier

```text
client                                  daemon
  │ Subscribe { stream, after_sequence? } │
  ├──────────────────────────────►│
  │                               │ snapshot/replay 可否を同じ registry lock で決定
  │◄── Subscribed { subscription }│
  │◄── Snapshot(base_sequence=N,   │  cursor 無し / replay 不可
  │             resource_revision)│
  │   または Event(N+1 ... M)      │  replay 可
  │◄── Event(M+1) ...              │
```

subscription 登録と snapshot/replay 起点の決定を同じ actor turn / critical section にし、snapshot と最初の delta の間を欠落させない。
client は `(stream key, last applied stream_sequence)` とresource固有cursorを別々に保存し、再接続時に渡す。
subscriber個別のfull snapshotはactorが捕捉したcurrent journal headを`base_sequence`として持ち、stream sequence自体を増やさない。
client Aのresyncがclient Bに実eventの無いgapを作らない。

- retained history に `after_sequence + 1` があればそこから replay する。
- sequence が古すぎる、未来、別 generation/resource の場合は `resync_required` を返し、delta を送らない。
- resync は full snapshot の `base_sequence` とresource cursorから reducer を置き換える。古いlocal stateへmergeしない。
- event sequence がlast applied以下なら重複として捨てる。1より大きいgapはdeltaを適用せずresyncする。
- resource revisionが古ければevent sequenceが新しくてもresource stateを巻き戻さない。

terminal output だけは revision を byte offset とする。`TerminalOutput { start_offset, end_offset, data }` の
`start_offset` が client cursor と一致しない場合、client は bytes を parser に入れず `TerminalSnapshot` を要求する。

## idempotency と long operation

### RequestId response cache と OperationId journal

`RequestId` は wire correlation と bounded response cache の key である。同じclient processがresponseを失い同じRPCを再送するときは
`(ClientId, RequestId)`を再利用できる。cache hitは同じresponseを返し、same key / different wire bodyは`idempotency_conflict`とする。
cache retentionはserver受信時刻からadvertised windowまでとし、UUID timestampやclient wall clockを信頼しない。

processをまたぐdurable idempotencyは`RequestId`ではなく`OperationId`が担う。session create/remove/setup、terminal spawn/kill、prompt
deliveryなどのintent producerは、request送信またはqueue publishより前に`OperationId`を発行する。CLI/MCP/TUI adapterはwait前にIDを
callerへ返せる形にし、daemonはtarget snapshot/recent operation listにもIDを載せる。daemon自身がqueue intentを作る場合もpublish前に発行する。

operation journal keyとconflict判定は次のtupleで行う。

```text
OperationKey = {
  operation_id,
  target_scope,                 # WorkspaceId / SessionId / TerminalRef ...
  semantic_body_digest
}
```

semantic digestはdecode済みtyped intentのcanonical formから作り、`RequestId`、timeout、connection、unknown ignored fieldを含めない。
同じ`OperationId`と同じscope/digestは別`ClientId`・別connection・別`RequestId`からでも同じoperationへ収束する。同じIDでscope/digestが
違えば`idempotency_conflict`とし、どちらも新しく実行しない。

journal compaction後もcompleted/failed/cancelled/ambiguous operationのtombstoneとdigestをretention policyまで残す。journalに存在しない
OperationIdを新mutationとしてadmitするのは、UUIDv7 timestampがserver時刻の`operation_admission_window_ms`内かつ
`max_future_skew_ms`以内の場合だけとする。それより古いIDはjournal無しでも`idempotency_expired`、未来過ぎるIDは`invalid_argument`として
effectを実行しない。client clockはadmissionでしか使わず、長時間operationと既存journal queryはwindowを超えても継続できる。
tombstoneは少なくとも、そのID timestampから`operation_admission_window_ms + max_future_skew_ms`を過ぎて新規admit不能になるまで保持する。

### side-effect reservation

spawn/create/remove/setup/prompt delivery は、外部 IO より先に受信した `OperationId`、semantic digest、durable reservationを保存する。

```text
request received
      │ persist operation key + reservation
      ▼
 accepted ──► running ──► succeeded
                  │       failed
                  │       cancelled
                  └─────► ambiguous
```

外部 side effect 後・結果 commit 前に crash した可能性があるときは `ambiguous` とし、blind retry しない。process identity、filesystem、
terminal registry、immutable plan を reconcile し、definitive outcome を証明できる場合だけ収束させる。

### operation API

| command | 結果 |
|---|---|
| `OperationGet { OperationId }` | latest state、revision、bounded progress、safe error/result |
| `OperationList { WorkspaceId, target?, recent? }` | reconnectしたclientが未確定operationを発見するbounded index |
| `OperationSubscribe { OperationId, cursor? }` | resume または snapshot + progress event |
| `OperationCancel { OperationId, expected_revision? }` | cancel request の受理。完了ではない |
| `OperationReconcile { OperationId }` | daemon reconciler の明示 wake-up。新 side effect は作らない |

operation state は `accepted / running / cancel_requested / succeeded / failed / cancelled / ambiguous` を分ける。
cancel は cooperative boundary で適用する。`OperationCancel` の response、client timeout、connection close のいずれも `cancelled` 完了を
意味しない。operation final state は journal/query/event だけで確定する。

## bounded resource と backpressure

初期 default は次の値とし、server hello で実効値を通知する。config で小さくできるが hard ceiling を超えられない。

| resource | default | overflow policy |
|---|---:|---|
| single frame | 1 MiB | allocation 前に protocol error、connection close |
| terminal output event payload | 64 KiB | 複数 event に分割 |
| terminal output journal | 8 MiB / terminal | oldest bytes を evict、遅れた client は resync |
| scrollback | 10,000 lines / terminal | daemon 設定で clamp、response は要求値と実効値を返す |
| scrollback response | 256 KiB decoded / response | windowをclampしcontinuation offsetを返す |
| terminal geometry | 500 cols × 200 rows | spawn/resize時にclampし実効値を返す |
| logical snapshot transfer | 16 MiB、1件 / connection | stagingを破棄して`resource_exhausted` / resync |
| client outbound control queue | 256 frame か 2 MiB | coalescible snapshot は最新版へ集約、それ以外は slow client を切断 |
| connections | 64 / uid、16 / client process | accept後hello前にも上限適用 |
| subscriptions | 64 / connection | side effect前に`resource_exhausted` |
| in-flight request | 128 / connection | side effect 前に `resource_exhausted` |
| terminal input batch | 64 KiB | `invalid_argument` |
| decoded base64 field | field固有上限以内 | decode前にencoded長、decode中にdecoded長を検査 |
| terminal input queue | 256 KiB / terminal | enqueue 前に `backpressure`、partial accept しない |
| handshake | 5 s | timeout close |
| stalled client write | 30 s | connection close、subscription cleanup |

control response/operation event と terminal output は別 queue/priority にする。PTY reader は client socket へ直接 `write_all` せず、必ず
daemon-owned journal へ append して PTY を drain する。各connectionはpartial-write中のframe bytesとwrite offsetを保持し、nonblocking
socketの`WouldBlock`後も同frameの残りから再開する。frame途中で次frameを混ぜない。clientはjournal cursorだけを持ち、遅いclientの
ために無制限copyを作らない。

snapshot 系 event は同じ stream の未送信旧 snapshot を新 snapshot で置換できる。response、error、operation terminal state、
`TerminalExited` は黙って drop/coalesce しない。送れなければ connection を閉じ、client はresponse cache／operation journal／stream cursorから
reconcileする。
coalesce/dropできるのはsocket writeをまだ開始していないframeだけである。write開始済みframeは完了またはconnection closeまで保持する。
terminal outputはper-client byte copyをoutbound control queueへ積まず、共有journalのrange descriptorとclient cursorをfair schedulerが処理する。

resource revisionは複数commitをまとめたsnapshotで正当にjumpできる。deltaをcoalesceする場合はstream sequence付与前にまとめるか、
current journal headの`Snapshot { base_stream_sequence, resource_revision }`としてreducer全置換を明示する。snapshot生成ではheadを増やさない。
sequenceが飛んだdeltaを「revisionが新しいから」と適用しない。

## disconnect・timeout・error taxonomy

### disconnect と timeout

| 事象 | daemon の扱い | client の扱い |
|---|---|---|
| clean/abrupt disconnect | connection と subscription を破棄。terminal/operation は継続 | cursor と unresolved RequestId/OperationId で再接続 |
| request wait timeout | response waiter だけ終了。accepted side effect は取消さない | 同processは同RequestId retry、別process/connectionは同OperationIdでGet/再要求 |
| deadline が side effect commit 前に満了 | request を実行せず `deadline_exceeded` | 新 intent が必要なら新 request |
| deadline が accepted 後に満了 | operation を継続し accepted/result を ledger に保持 | OperationGet/Cancel |
| ACK response loss | outcome unknown、metadata/claim を保持 | 同 ID retry/reconcile。成功と仮定しない |
| daemon socket unavailable | active daemon を autospawn。失敗は `unavailable` | managed local fallback しない |

request の `timeout_ms` は server 処理の相対上限で、server hello の最大値へ clamp する。absolute wall clock deadline は clock skew を
持ち込むため wire に置かない。liveness は elapsed time だけで判断せず、process/lock/operation evidence と合わせる。

### error envelope

error は `code`、`message`、`retry_mode` (`never / same_request / same_operation / reconnect / resync / manual`)、
`side_effect` (`none / operation_accepted / applied / partial_or_unknown`)、任意のsafe typed details、`error_id`を持つ。
OS error、stderr、secret、raw prompt/input を wire や通常 log に出さない。

| code | 意味 | retry policy |
|---|---|---|
| `protocol_mismatch` / `capability_missing` | handshake 非互換 | client/build 更新。自動 retry しない |
| `unauthenticated` / `permission_denied` | peer/workspace/runtime token 不正 | retry しない |
| `invalid_argument` | schema/size/name/transition 不正 | 入力修正 |
| `not_found` | definitive に resource 不在 | stale metadata cleanup 可 |
| `stale_target` | ID scope/incarnation/generation 不一致 | fresh snapshot を取得。別 resource へ置換しない |
| `generation_rolled_over` | control endpoint が current でない | details の current locator へ handshake |
| `revision_conflict` | expected revision/state 不一致 | snapshot 後に intent を再評価 |
| `idempotency_conflict` | same key/different body | client bug。retry しない |
| `idempotency_expired` | input result window / operation tombstone retention 外 | resource snapshot/listでreconcile。blind replayしない |
| `resource_exhausted` / `backpressure` | bounded limit 到達 | server hint 後に bounded retry |
| `busy` | lifecycle/operation/generation transition 中 | revision/event を待つ |
| `deadline_exceeded` | commit 前 timeout または wait timeout | `side_effect` を見て retry/reconcile |
| `cancelled` | operation の terminal state | retry は新 intent |
| `ownership_unknown` | orphan、ACK loss、registry corruption | replacement spawn/kill を止め reconcile |
| `unavailable` | daemon/adapter 一時不通 | bounded autospawn/reconnect |
| `internal` | safe details にできない server failure | `error_id` を提示し、mutation は reconcile |

typed attach failure は `not_found`（definitive missing）と `ownership_unknown`（orphan/adopted/registry 不明）へ統合する。
fresh replacement が許されるのは前者だけである。
