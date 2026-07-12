# 5. daemon

> [ドキュメント目次](README.md) ｜ ← 前へ [4. daemon IPC](04-ipc.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

managed session と terminal を所有する daemon の現在の契約である。IPC wire と transport は
[4. daemon IPC](04-ipc.md) を正本とする。

## 目次

- [authority と lifecycle](#authority-と-lifecycle)
- [durable operation](#durable-operation)
- [terminal ownership](#terminal-ownership)
- [generation と orphan safety](#generation-と-orphan-safety)

## authority と lifecycle

managed session の lifecycle state は daemon が単一書き手である。CLI、MCP、TUI は command を提出し、
legacy `state.json` を managed state として解釈・更新しない。lifecycle state は `creating`、
`initializing`、`available`、`deleting`、`failed` の closed vocabulary であり、Agent phase と branch
status は別軸として保持する。

daemon は lifecycle reducer の event を durable store の lock 下で適用する。各 accepted event は
`state_revision` を増やす。create と remove は operation を予約してから進めるため、remove 後に同名を
再作成しても別の `SessionId` になり、古い worker は新しい session を更新できない。

## durable operation

operation journal は operation ID、owner daemon generation、execution attempt、progress revision、status
を保存する。status は `accepted`、`running`、`cancel_requested`、`succeeded`、`failed`、`cancelled`、
`ambiguous` である。terminal status になった operation を同じ ID で restart しない。

durable store は、受理される create / remove operation の owner generation が daemon と一致することを
検証する。completion は `CompletionFence` と reducer transition の両方を満たす場合だけ反映される。
このため ACK loss や late worker で effect の結果を推測して二重実行しない。

## terminal ownership

terminal registry は daemon generation が所有する `TerminalRef` を key にする。attach は snapshot と
subscription を atomically 作り、detach と client disconnect は当該 connection の attachment だけを
外す。PTY、output journal、process ownership は client disconnect では解放しない。

raw output は bounded journal に offset を付けて保持する。attach client は snapshot の後、連続する
output offset を適用する。journal に残らない cursor、sequence gap、epoch mismatch は resync を要求する。
terminal input は `(ClientId, TerminalRef, input sequence, RequestId)` で dedupe し、同じ input batch を
別 connection から重複 write しない。input は queue capacity を予約してから enqueue し、ACK は全 byte が
PTY endpoint に書き込まれた後だけ返す。partial write は ambiguous として扱う。

terminal resize は registry の revision と geometry を更新する。terminal exit は final output を append
してから exited state を記録するため、ownership を early release しない。

## generation と orphan safety

generation coordinator は active daemon を一つだけ持つ。active generation だけが session/control mutation
と新規 terminal spawn を行う。rollover は next generation が standby の状態から active になり、previous
active は draining になる。running non-terminal external IO がある場合の rollover は `busy` で拒否する。

draining generation は自 generation が所有する terminal の attach、input、output、exit を完了できるが、
session/control state は書かない。terminal endpoint は `TerminalRef` に含まれる owner generation の trusted
record からだけ解決する。

spawn reservation は process spawn より先に保存する。crash 後に process identity を証明できない terminal は
`identity_unknown` として扱い、replacement spawn、input、kill を自動で行わない。PID の生存だけでは ownership
を証明しない。daemon crash をまたぐ PTY master FD の継続はこの契約に含めず、
[PTY broker／FD handoff の調査](proposals/07-pty-crash-continuation.md) に分離する。
