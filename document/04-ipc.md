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
revision の共通範囲と必須 capability を検証し、成功時に `ServerHello` を返す。build identity は
診断情報であり、互換性の判定には使わない。通常 envelope は handshake の成功後だけ受理する。

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

## managed session request

`session` kind の `create`、`remove`、`list`、`overview` は daemon が所有する durable lifecycle runtime に届く。create / remove は producer-issued `OperationId` を accepted response に返し、list / overview は同じ revision 付き workspace snapshot を返す。`OperationId` の再送は action と canonical session target が一致するときだけ同じ operation を返し、異なれば `idempotency_conflict` で拒否する。

snapshot の session は `WorkspaceId`、`SessionId`、`WorktreeId`、lifecycle を含む。agent / terminal 起動用の checkout path は、daemon が available の完全一致 scope からだけ解決する。client が name または path を渡して scope を再探索する wire contract はない。

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
