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
| `schema_version` | `u16` | metrics payload schema version。現在は `1` |
| `sampled_at_ms` | `u64` | daemon が sample を作成した monotonic timestamp |
| `cpu_percent_hundredths` | `u32` | 前回 sample からの daemon process CPU 使用率（百分率の 1/100 単位） |
| `resident_memory_bytes` | `u64` | daemon process の peak resident memory（byte） |
| `active_subscribers` | `u32` | sample 作成時の observer 数 |
| `dropped_updates` | `u64` | slow observer の bounded queue で coalesce した update 数 |

各 subscriber は容量 1 の queue を持つ。daemon は tick で block せず、queue が埋まった
observer の中間 sample を落として count する。切断された observer は次の publish で取り除く。
このため遅い TUI や一つの接続の切断が daemon tick または他 TUI の配信を止めない。

## managed session request

`session` kind の `create`、`remove`、`list`、`overview` は daemon が所有する durable lifecycle runtime に届く。create / remove は producer-issued `OperationId` を accepted response に返し、list / overview は同じ revision 付き workspace snapshot を返す。create / remove の accepted response は snapshot とともに safe final hook を返す。hook は `kind`（`session.created` または `session.removed`）、`operation_id`、`revision` を持ち、TUI は create skeleton を同じ operation の `session.created` hook でだけ終了する。`OperationId` の再送は action と canonical session target が一致するときだけ同じ operation を返し、異なれば `idempotency_conflict` で拒否する。

snapshot の session は `WorkspaceId`、`SessionId`、`WorktreeId`、lifecycle を含む。agent / terminal 起動用の checkout path は、daemon が available の完全一致 scope からだけ解決する。client が name または path を渡して scope を再探索する wire contract はない。

## agent launch request

`agent` kind は daemon 所有の Agent runtime に届く。client は producer-issued `OperationId` と、`WorkspaceId` / `SessionId` / optional profile ID だけの launch intent を送る。worktree、checkout path、profile 既定値、argv、environment、secret は wire field ではなく、daemon が [managed session scope](05-daemon.md#authority-と-lifecycle) と code-defined adapter registry から解決する。profile を省略すると daemon の既定 policy が選ぶ。

daemon は intent の `(WorkspaceId, SessionId)` を [available managed session](05-daemon.md#authority-と-lifecycle) の完全一致 scope に解決し、その worktree だけを launch に使う。creating / deleting / failed / stale / mismatch の scope、未知 profile、canonical でない `OperationId` は PTY を spawn せず typed safe error になる。

成功した launch は accepted response に producer `OperationId` と durable revision を返し、body に完全な `TerminalRef` を載せる。この `TerminalRef` は operation・workspace・session・worktree・daemon generation・terminal incarnation を fence する。PTY exit を daemon が一度だけ記録すると、同じ semantic intent の再送は成功時に `completed: true` と同じ `TerminalRef` を持つ final response を返す。non-zero exit は安全な `unavailable` final として replay される。同じ `OperationId` を異なる intent で送ると `idempotency_conflict` になる。spawn failure・ambiguous・persist-after-spawn は fenced safe failure（`unavailable` / `ownership_unknown`）として durable に記録され、resend は同じ安全な失敗を replay する。replacement spawn や terminal の推測は行わない。

Agent の pending pane は、同じ `OperationId` の成功 final が返した `TerminalRef` にだけ attach する。attach 以降の stream（`attach` / `resume` / `resync` / `input` / `resize` / `detach`）は [generic terminal request](#generic-terminal-request) と同じ vocabulary を共有し、daemon は `TerminalRef` の所有元（agent または generic）へ透過的に routing する。この pending pane の attach policy は [3. TUI](03-tui.md) を正本とする。

## dispatch request

`dispatch` は managed session の既存 create lifecycle と Agent launch を合成する即時実行 request である。payload は producer-issued `operation_id`、workspace、session name、execution context から得た caller、排他的な worker selector（既存 `agent_id` または `runtime` と `model`）、prompt を持つ。daemon は session を reuse/create して available scope を確認してから、prompt を `initial_prompt` として launch する。成功 reply は Accepted outcome と `run_id`（operation ID）および fenced terminal を返す。同じ operation の再送は同じ outcome を返し、異なる intent は idempotency conflict である。

client は path、argv、queue/live mode、completion destination を指定しない。available でない session scope、agent selector の不整合、または未知 agent は safe typed error となり PTY を spawn しない。

## generic terminal request

generic terminal の request vocabulary は `terminal` kind の `launch`、`inventory`、`attach`、
`resume`、`resync`、`input`、`resize`、`detach` である。launch は stable profile ID、
`WorkspaceId` / optional `SessionId` / `WorktreeId` の scope、geometry だけを送る。command、argv、
working directory、environment、secret は wire field ではなく、daemon が trusted profile から解決する。

launch の response は完全な `TerminalRef` を返す。attach は snapshot と connection-owned
subscription を同時に返す。input、resize、detach はその `TerminalRef` と subscription を必ず含める。
output は `(start_offset, end_offset)` の連続範囲で表し、resume の cursor が journal 外なら
`resync_required` を返す。`stale_target`、`ownership_unknown`、partial write を含む安全に証明
できない結果は typed error であり、client は local PTY を生成しない。

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

MCP の dispatch request は `DispatchTool` action として送る。daemon が session upsert、agent/run/binding
の解決、inbox の読み書きを行い、MCP は durable state を直接読んだり書いたりしない。完了・失敗は worker
の current run と binding が一意に一致するときだけ配送し、不一致は completion fence と同じ fail-closed
方針で no-op にする。
