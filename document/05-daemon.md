# 5. daemon

> [ドキュメント目次](README.md) ｜ ← 前へ [4. daemon IPC](04-ipc.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

managed session と terminal を所有する daemon の現在の契約である。IPC wire と transport は
[4. daemon IPC](04-ipc.md) を正本とする。

## 目次

- [authority と lifecycle](#authority-と-lifecycle)
- [daemon process lifecycle](#daemon-process-lifecycle)
- [daemon data directory](#daemon-data-directory)
- [durable operation](#durable-operation)
- [terminal ownership](#terminal-ownership)
- [generation と orphan safety](#generation-と-orphan-safety)

## authority と lifecycle

managed session の lifecycle vocabulary は daemon のために定義されている。CLI、MCP、TUI は command を
提出し、legacy `state.json` を managed state として解釈・更新しない。lifecycle state は `creating`、
`initializing`、`available`、`deleting`、`failed` の closed vocabulary であり、Agent phase と branch
status は別軸として保持する。

durable reducer と store は accepted event ごとに `state_revision` を増やし、create/remove を operation と
session incarnation で fence する。IPC の create/remove は daemon が reservation を永続化してから worktree
effect を実行し、同じ daemon generation・operation・session attempt・revision の completion だけを反映する。
失敗した effect は safe failure として残り、client が local worktree 操作へ fallback しない。

## daemon process lifecycle

`usagi daemon` は daemon 面の process lifecycle を操作する入口である。通常の client は、接続先が
無いときに `start` を一度だけ起動して endpoint の公開を待つ。client が daemon-owned terminal や
managed session をローカルに代替実行することはない。

| コマンド | 動作 |
|---|---|
| `usagi daemon start` | detached `serve` を起動し、`daemon.json` に稼働中の pid が登録されるまで待つ。すでに稼働中なら新しい process を起動しない |
| `usagi daemon status` | lifecycle record と pid の生存判定から running / stale / absent を表示する |
| `usagi daemon stop` | 稼働中 daemon に終了を要求して lifecycle record を消去する。stale record は process に signal を送らず消去する |
| `usagi daemon restart` | 稼働中 daemon を停止してから新しい daemon を起動する |
| `usagi daemon` / `usagi daemon serve` | 前景で daemon を serve する。`serve` は内部用の subcommand である |

`serve` は process lifetime にわたって単一インスタンス lock を保持する。record は daemon の発見に使う
補助情報であり、単一インスタンスの権威は lock である。

## daemon data directory

daemon の process lifecycle と Unix transport は `<data-dir>/daemon/` を使う。これは daemon の
内部状態であり、利用者が編集する設定ファイルではない。

| path | 種別 | 用途 |
|---|---|---|
| `daemon.json` | JSON | 稼働中 daemon の pid と登録時刻。daemon は起動時に書き、正常終了時に消去する |
| `daemon.lock` | lock file | `serve` が保持する単一インスタンス lock。process 終了時に OS が解放する |
| `current.json` | JSON locator | active daemon generation の Unix socket endpoint を atomically 公開する |
| `generations/<generation>/sock` | Unix domain socket | generation ごとの IPC endpoint。socket と locator は所有者・permission・symlink を検証して利用する |

`daemon.json` は `pid` と `started_at` を持つ。`current.json` は generation、daemon directory からの
相対 endpoint、`active` または `draining` の state を持つ。socket endpoint は永続データではなく、
daemon generation の終了とともに消える。

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

generic shell terminal は IPC connection handler が所有 runtime へ渡す。runtime は generic terminal
coordinator、trusted profile resolver、durable terminal store、injected PTY adapter を一つの ownership
loop に保持する。connection close は runtime に通知して当該 connection の subscription だけを外し、
profile resolution や replacement spawn を行わない。

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
