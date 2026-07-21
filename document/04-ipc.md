# 4. daemon IPC

> [ドキュメント目次](README.md) ｜ ← 前へ [3. TUI](03-tui.md) ｜ 次へ → [5. daemon](05-daemon.md)

daemon と各 client 面が共有する IPC の現在の契約である。クレート境界と実装の置き場所は
[2. アーキテクチャ](02-architecture.md) を正本とする。

## 目次

- [identity と fence](#identity-と-fence)
- [frame と handshake](#frame-と-handshake)
- [envelope とエラー](#envelope-とエラー)
- [Unix transport](#unix-transport)
- [client の失敗処理](#client-の失敗処理)
- [managed session request](#managed-session-request)
- [daemon metrics subscription](#daemon-metrics-subscription)
- [PR inventory snapshot](#pr-inventory-snapshot)
- [agent launch request](#agent-launch-request)
- [dispatch request](#dispatch-request)
- [generic terminal request](#generic-terminal-request)

## identity と fence

v2 の resource identity は lowercase canonical UUID の newtype である。表示名、path、PID、
daemon 内 counter は属性であり、effect を行う resource key ではない。`WorkspaceId`、`SessionId`、
`WorktreeId`、`TerminalId`、`AgentRuntimeId`、`DaemonGeneration` は resource incarnation ごとに
新規発行される。`OperationId` は UUIDv7 の durable intent identity である。

effecting terminal command は完全な `TerminalRef` を使う。これは daemon generation、terminal、
workspace、optional session、worktree の全 ID を含む。一つでも registry の entry と異なれば
`stale_target` であり、名前・path・単独 terminal ID による再探索はしない。Agent runtime も
`AgentRuntimeRef` で terminal と session に束縛する。

late worker completion は workspace、session、operation、owner generation、execution attempt、
lifecycle attempt、expected revision を含む `CompletionFence` を照合してから適用する。不一致の
completion は state mutation にしない。legacy state は typed incarnation を持たないため、managed
session state へ推測移行しない。

## frame と handshake

transport は u32 big-endian length prefix と JSON payload の frame を運ぶ。空 frame、negotiated
上限を超える frame、途中まで読んだ prefix の EOF はエラーである。prefix の前に EOF となった
場合だけ clean close とする。既定 frame 上限は 1 MiB であり、reader は長さを検証してから
payload を確保する。

最初の frame は必ず `ClientHello` である。hello は client ID、connection nonce、期待する
daemon generation、対応 protocol range、capability、build diagnostics を含む。daemon は generation /
revision の共通範囲と必須 capability を検証し、成功時に `ServerHello` を返す。build identity は wire
protocol の互換性判定には使わないが、client bootstrap は `ServerHello` の identity で同一 channel の
daemon が現在 binary と同じ build かを確認し、異なる build を lifecycle rollover する。通常 envelope は
handshake の成功後だけ受理する。

## envelope とエラー

通常通信は protocol version と daemon generation を必ず持つ envelope である。

| kind | 相関子 | 用途 |
|---|---|---|
| request | `RequestId` | client の一回の RPC |
| response | 同じ `RequestId` | immediate result、accepted operation、または typed error |
| event | `SubscriptionId`、`StreamRef`、sequence | server push |

`RequestId` の response cache は `(ClientId, RequestId, body digest)` を照合する。同じ ID を別 body に
再利用すると `idempotency_conflict` になる。durable mutation は request correlation と独立した
`OperationId` を持ち、target scope と semantic digest が同じ場合だけ既存 operation として再利用する。

`ProtocolError` は machine-readable な code、safe message、retry mode、side-effect classification、
error ID を返す。resource/ownership を証明できない場合は `ownership_unknown`、resume が成立しない
場合は `resync_required` を使う。OS error、secret、raw launch provision は error detail に含めない。

## daemon metrics subscription

`metrics` request は TUI が daemon の観測用 stream を登録または解除するための control
vocabulary である。`subscribe` は TUI 起動時および接続を回復した後に送り、正常終了時には
`unsubscribe` を送る。接続が切れた subscription は connection-local であり、再接続で resume
せず新しく登録する。

daemon が送る snapshot は次の versioned schema である。これは表示・診断専用で、TUI が
session / terminal の所有権や local fallback を判断する根拠にはしない。

| field | type | meaning |
|---|---|---|
| `schema_version` | `u16` | metrics payload schema version。現在は `2` |
| `sampled_at_ms` | `u64` | daemon が sample を作成した monotonic timestamp |
| `cpu_percent_hundredths` | `u32` | 前回 sample からの daemon process CPU 使用率（百分率の 1/100 単位） |
| `resident_memory_bytes` | `u64` | daemon process の peak resident memory（byte） |
| `active_subscribers` | `u32` | sample 作成時の observer 数 |
| `dropped_updates` | `u64` | slow observer の bounded queue で coalesce した update 数 |
| `terminal_dropped_bytes` | `u64` | retention window から trim した terminal output byte 数 |
| `terminal_coalesced_bytes` | `u64` | retained segment に連結した terminal output byte 数 |
| `terminal_backpressured_bytes` | `u64` | bounded PTY observation queue の空きを待った terminal output byte 数 |

各 subscriber は容量 1 の queue を持つ。daemon は tick で block せず、queue が埋まった
observer の中間 sample を落として count する。切断された observer は次の publish で取り除く。
このため遅い TUI や一つの接続の切断が daemon tick または他 TUI の配信を止めない。

## PR inventory snapshot

`pr` request は stable `SessionId` を対象に daemon-owned inventory の source-of-truth snapshot を返す。
handshake では `pr.snapshot.v1` capability を必須にし、dedicated subscription を提供する peer は
`pr.subscription.v1` も advertise する。

| action / event | fields | contract |
|---|---|---|
| `snapshot` | `session_id`, `revision?` | canonical URL、optional title、state、pin/dismiss と refresh state を含む current snapshot を返す |
| `subscribe` / `unsubscribe` | `session_id` | connection-local hint subscription を登録・解除する。disconnect は登録を回収する |
| `pr.updated` | `session_id`, `revision` | inventory mutation を示す lossy hint。client は snapshot を再取得して収束する |

revision は session ごとに monotonic である。duplicate、欠落、順序逆転した `pr.updated` は client state
の差分適用根拠にしない。client は最後に見た revision より新しい hint を受けた場合、または reconnect 後に
snapshot を読み直す。slow subscriber は bounded queue で coalesce/drop され、PR refresh、terminal drain、
他 client の RPC を停止させない。

## managed session request

`session` kind の `create`、`remove`、`list`、`overview`、`resume_agent` は daemon が所有する durable lifecycle / Agent runtime に届く。create / remove / resume_agent は producer-issued `OperationId` を accepted response に返し、list / overview は同じ revision 付き workspace snapshot を返す。create / remove の accepted response は snapshot とともに safe final hook を返す。hook は `kind`（`session.created` または `session.removed`）、`operation_id`、`revision` を持ち、TUI は create skeleton を同じ operation の `session.created` hook でだけ終了する。`OperationId` の再送は action と canonical session target が一致するときだけ同じ operation を返し、異なれば `idempotency_conflict` で拒否する。

create / remove の durable outcome と wire response / hook の対応は次の表を正本とする。同じ semantic operation の再送は daemon restart の前後を問わず同じ行を replay し、filesystem / Git effect を再実行しない。

| durable outcome | IPC outcome | final hook |
|---|---|---|
| `succeeded` | `accepted`（同じ `operation_id` / final revision / snapshot） | create は `session.created`、remove は `session.removed` |
| `failed`（effect failure または interrupted reconcile） | safe `error` | なし |
| 同じ `OperationId`、異なる action / canonical target | `idempotency_conflict` | なし |

snapshot の session は `WorkspaceId`、`SessionId`、`WorktreeId`、lifecycle を含み、workspace 全体の **root `WorktreeId`**（`⌂ root` の scope 識別子）も含む。agent / terminal 起動用の checkout path は、daemon が available の完全一致 scope（managed session、または `session_id` を持たない workspace root）からだけ解決する。client が name または path を渡して scope を再探索する wire contract はない。

## agent launch request

`agent` kind は daemon 所有の Agent runtime に届く。client は producer-issued `OperationId` と、`WorkspaceId` / optional `SessionId`（省略時は workspace root）/ optional profile ID だけの launch intent を送る。worktree、checkout path、profile 既定値、argv、environment、secret は wire field ではなく、daemon が [managed session scope](05-daemon.md#authority-と-lifecycle) と code-defined adapter registry から解決する。profile を省略すると daemon の既定 policy が選ぶ。

daemon は intent の `(WorkspaceId, SessionId?)` を [available scope](05-daemon.md#authority-と-lifecycle) の完全一致に解決し、その worktree だけを launch に使う。`SessionId` を省略した intent は workspace root に解決し、cwd を trusted repository root にする。creating / deleting / failed / stale / mismatch の scope、未知 profile、canonical でない `OperationId` は PTY を spawn せず typed safe error になる。

成功した launch は accepted response に producer `OperationId` と durable revision を返し、body に完全な `TerminalRef` を載せる。この `TerminalRef` は operation・workspace・session・worktree・daemon generation・terminal incarnation を fence する。PTY exit を daemon が一度だけ記録すると、同じ semantic intent の再送は成功時に `completed: true` と同じ `TerminalRef` を持つ final response を返す。non-zero exit は安全な `unavailable` final として replay される。同じ `OperationId` を異なる intent で送ると `idempotency_conflict` になる。spawn failure・ambiguous・persist-after-spawn は fenced safe failure（`unavailable` / `ownership_unknown`）として durable に記録され、resend は同じ安全な失敗を replay する。replacement spawn や terminal の推測は行わない。

この replay 契約は daemon restart をまたぐ。daemon は Agent snapshot の load と operation ledger hydrate が完了するまで request admission を開始せず、成功 final、non-zero exit、safe failure を同じ意味で返す。restart 時に所有権を証明できない未終端 runtime は `identity_unknown` として inventory に `live: false` で現れ、attach / input / kill / replacement spawn を拒否する。snapshot が破損または未知 schema の場合は daemon startup が fail closed となり、Agent spawn と snapshot 更新を行わない。MCP caller credential は replay 対象ではなく restart で失効する。

Agent の pending pane は、同じ `OperationId` の成功 final が返した `TerminalRef` にだけ attach する。attach 以降の stream（`attach` / `resume` / `resync` / `input` / `resize` / `detach`）は [generic terminal request](#generic-terminal-request) と同じ vocabulary を共有し、daemon は `TerminalRef` の所有元（agent または generic）へ透過的に routing する。この pending pane の attach policy は [3. TUI](03-tui.md) を正本とする。

## provider conversation resume request

`SessionAction::ResumeAgent` は利用者が明示的に開始する provider conversation の再開である。payload は canonical `operation_id`、`workspace_id` と、stable `session_id` または session name を持つ。provider-native ID、profile argv、environment、transcript、旧 `TerminalRef` は wire field に含めない。成功時は daemon が新しく所有する `AgentRuntimeRef` / PTY incarnation の完全な `TerminalRef` を返す。これは旧 PTY の stream `resume` や再 attach ではない。

同じ operation と同じ intent の再送は同じ admission を replay し、別 intent は `idempotency_conflict` になる。失敗はsafe `invalid_argument` / `unavailable` / `ownership_unknown`となる。status / overview の projection は `agent_phase: interrupted`、`agent_resumable` と非機密な `agent_resume_reason` だけを返し、native ID はIPC・hook・errorへ出さない。provider capture、scope / revision / live fence、redaction、new PTY spawnの正本は[Provider-native conversation resume](05-daemon.md#provider-native-conversation-resume)とする。

daemon restart、TUI起動、workspace open時のpane復元は `ResumeAgent` を送らない。CLI `usagi session resume <name>`、TUI `session resume <name>`、MCP `session_resume` の明示操作だけがこの request を作る。

## dispatch request

`dispatch` は managed session の既存 create lifecycle と Agent launch を合成する即時実行 request である。payload は producer-issued `operation_id`、workspace、session name、execution context から得た caller、排他的な worker selector（既存 `agent_id` または `runtime` と `model`）、prompt を持つ。daemon は session を reuse/create して available scope を確認してから、prompt を `initial_prompt` として launch する。成功 reply は Accepted outcome と `run_id`（operation ID）および fenced terminal を返す。同じ operation の再送は同じ outcome を返し、異なる intent は idempotency conflict である。

dispatch の operation key、caller↔worker binding、runtime generation、safe outcome も restart 時に hydrate される。同じ dispatch の retry は worker を再 spawn せず、保存済み outcome を replay する。

client は path、argv、queue/live mode、completion destination を指定しない。available でない session scope、agent selector の不整合、または未知 agent は safe typed error となり PTY を spawn しない。新規 agent の runtime/model は daemon が launch 直前に current workspace allowlist と current executable availability で再検証する。allowlist 外は `invalid_argument`、executable 不在は `unavailable` とし、どちらも PTY を spawn しない。

## generic terminal request

generic terminal の request vocabulary は `terminal` kind の `launch`、`inventory`、`attach`、
`resume`、`resync`、`input`、`resize`、`detach` である。launch は stable profile ID、
`WorkspaceId` / optional `SessionId` / `WorktreeId` の scope、geometry だけを送る。command、argv、
working directory、environment、secret は wire field ではなく、daemon が trusted profile から解決する。

launch の response は完全な `TerminalRef` を返す。attach は snapshot と connection-owned
subscription を同時に返す。input、resize、detach はその `TerminalRef` と subscription を必ず含める。
terminal command の effect は、daemon generation、terminal、workspace、optional session、worktree、
runtime ownership/state の全 fence を read-only で検証した後だけ実行する。resize はこの preflight から
PTY effect、geometry commit まで terminal actor の排他区間を保持するため、途中の exit/replacement は
割り込まない。PTY effect が失敗した場合は `unavailable` を返し、committed geometry を更新しない。
output は `(start_offset, end_offset)` の連続範囲で表す。attach / resync snapshot は retention
window の先頭 `base_offset`、末尾 `output_offset`、その半開区間 `[base_offset, output_offset)` の
`replay` を返し、常に `base_offset + replay.length == output_offset` を満たす。window は最大 64 KiB
であるため、byte array の JSON 展開と response envelope を含めても既定 1 MiB frame 上限内に収まる。

resume は `after_offset` が window より古い場合、または `output_offset` より未来の場合に
`resync_required` を返す。window 内の segment
途中を指す場合は、その offset から始まる suffix を返し、最初の `start_offset` は必ず
`after_offset` と一致する。client は `resync_required` 後に snapshot で画面を置換し、返された
`output_offset` から resume する。同じ古い cursor を再送しない。この `base_offset` は protocol
generation 1 revision 1 の additive field であり、revision 1 client は必須 field として検証する。

`stale_target`、`ownership_unknown`、partial write を含む安全に証明
できない結果は typed error であり、client は local PTY を生成しない。

terminal input は daemon が PTY master に受理された byte 数を追跡し、operation の outcome として保持する。
同じ client の同じ `input_seq` と request identity を再送した場合は保存済み outcome を replay し、PTY へ再送しない。

| PTY write outcome | input ack | retry contract |
|---|---|---|
| 全 byte を適用 | `Written` | 同一 operation の再送は `Cached(Written)` |
| 適用済み prefix が 0 byte の failure | `Failed` | effect がないため、新しい operation として安全に再試行できる |
| 1 byte 以上を適用後の failure / `WriteZero` | `Ambiguous { applied_prefix }` | 同一 operation の再送は `Cached(Ambiguous { applied_prefix })` とし、既適用 byte を暗黙に再送しない |

PTY write が `Interrupted` を返した場合、daemon はそれまでの `applied_prefix` を維持して残りを再試行する。
wire 型は既存の `applied_prefix` を使うため protocol revision の変更を伴わない。

`inventory` は `WorkspaceId` / optional `SessionId`（None=root）/ `WorktreeId` の scope を送り、
その scope に**完全一致**する daemon 所有 runtime を列挙する。daemon は generic terminal owner と
Agent owner の両方に問い合わせて結果を merge するため、応答には**generic terminal と Agent terminal の
両方**が含まれる。各エントリは完全な `TerminalRef`、`kind`（`terminal` / `agent`）、`live`（現 daemon
generation が所有し attach 可能か）だけを持ち、argv・environment 値・secret・provider transcript は
含めない。`exited`・reconcile 中・orphan の runtime は `live: false` として返り attachable にはならない。
client はこの列挙で発見した live runtime にだけ、その `TerminalRef` で fenced に attach する
（名前や path から terminal を推測しない）。workspace open 時の pane 復元でこの列挙を使う（[3. TUI](03-tui.md#workspace-open-時の-pane-復元) を正本とする）。

daemon restart 後も `inventory` は `terminals.json` から復元した generic terminal record を同じ scope と
`TerminalRef` のまま返す。ただし旧 daemon の PTY master は復元しないため、未終端 record は
`identity_unknown`、`live: false` となる。旧 ref の attach、resume、resync、input、resize、detach は
typed safe error となり、別 terminal の PTY effect や暗黙の replacement spawn を起こさない。restart 時の
永続化・破損時の扱いは [5. daemon](05-daemon.md#daemon-data-directory) を正本とする。

## Unix transport

Unix socket は daemon 専用 adapter が管理する。endpoint は private data directory の generation
directory に作り、bind 成功後に current locator を atomic publish する。directory は `0700`、socket と
locator は `0600` で、所有 UID・mode・symlink でないことを discovery と accept の両方で検証する。

accept 時は OS peer credential の UID が daemon UID と一致しなければ、protocol byte を読む前に接続を
閉じる。client は active locator だけを解決でき、draining locator や generation directory 外を指す
endpoint には接続しない。

## client の失敗処理

TUI、CLI、MCP は共通 daemon client port を通して managed session と terminal の要求を送る。接続失敗、
protocol error、ownership unknown は local managed PTY や local session mutation への fallback を許可しない。

retry は `ProtocolError` の retry mode に従う。mutation を再送するときは元の request / operation identity
を保持する。TUI は stream sequence、resource revision、terminal output offset を別々に保持し、gap や
epoch の不一致では output を継ぎ足さず、snapshot resync を要求する。

terminal の `unavailable` は connection-local subscription の喪失として扱う。TUI は 100ms から 2s 上限の
指数 backoff で transport を開き直し、元の完全な `TerminalRef` に `attach` して atomic snapshot と新しい
subscription を取得する。成功後は snapshot の `output_offset` から `resume` し、backoff と subscription-local
input sequence を reset する。`stale_target`、`ownership_unknown`、exited は retry 対象ではなく、detach / tab
close も pending retry を解除する。どの失敗経路も replacement launch を行わない。

terminal input は Live な connection-owned subscription がある場合だけ送る。非 Live、subscription 不在、または
request failure は typed failure であり、client は success として捨てず未配送 feedback を表示する。再接続まで
入力を queue / replay しないため、遅延送信や二重送信は生じない。

MCP の dispatch request は `DispatchTool` action として送る。daemon が session upsert、agent/run/binding
の解決、inbox の読み書きを行い、MCP は durable state を直接読んだり書いたりしない。完了・失敗は worker
の current run と binding が一意に一致するときだけ配送し、不一致は completion fence と同じ fail-closed
方針で no-op にする。
