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
- [agent ownership](#agent-ownership)
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

各 managed session は `SessionId` と `WorktreeId` を同時に永続化する。agent / delegation が必要とする path は、available の workspace / session / worktree identity がすべて一致する場合だけ daemon が返す。creating、deleting、failed、stale identity、表示名・path-only の指定は scope に解決しない。

## daemon process lifecycle

`usagi daemon` は daemon 面の process lifecycle を操作する入口である。すべての TUI 起動、daemon-owned
CLI operation、MCP server は共有 bootstrap を通る。release binary は同じ build identity の active endpoint を
再利用し、異なる build は lifecycle restart と readiness / handshake 確認を経て切替える。debug binary は
development channel を使い、ローカルの `cargo run` 起動時だけ同 build daemon も restart する。test harness や
直接実行した debug binary は同 build daemon を再利用する。locator はあるが接続不能・draining・不正な
場合は replacement を起動せず、安全な typed lifecycle error を表示する。client が daemon-owned terminal や
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
| `terminals.json` | JSON | generic terminal の launch reservation、trusted profile provenance、process identity、runtime state。PTY master と output journal は process memory にのみ保持する |
| `agents.json` | JSON | Agent runtime の launch reservation、public launch plan snapshot、process identity、runtime state。argv や secret を含む adapter private provision と PTY master は永続化しない |

`data_dir` は release では `$USAGI_HOME` または `~/.usagi`、debug ではその `development/` 子 directory である。
したがって `cargo run` は production の record / locator / lock / daemon-owned state に触れず、
`cargo run --release` は従来の production channel を使う。`USAGI_HOME` を明示しても同じ分離を適用する。

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

daemon 起動時には未完了の create / initialize / delete journal を reconcile する。physical effect の完了を証明できない record は再実行せず safe failure にして明示 recovery を待つ。

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

generic shell terminal は root IPC server が全 connection で共有する ownership runtime へ渡す。runtime は
generic terminal coordinator、trusted `login-shell` profile resolver、durable terminal store、実 PTY adapter
を一つの ownership loop に保持する。PTY reader は output journal へ drain され、connection close は runtime
に通知して当該 connection の subscription だけを外し、profile resolution や replacement spawn を行わない。

## agent ownership

Agent runtime は daemon 所有の Agent owner が持つ。owner は durable runtime coordinator、Codex / Claude
adapter を解決する code-defined adapter registry、durable runtime store、実 PTY adapter、producer-issued
`OperationId` の idempotency ledger を一つに束ねる。[`agent` launch request](04-ipc.md#agent-launch-request)
は [managed session scope](#authority-と-lifecycle) を解決してから registry で profile を選び、adapter が
one-shot で provision した public launch plan だけを durable snapshot に保存する。argv、environment value、
secret、raw provision error は wire event・snapshot・TUI feedback に現れない。

launch は reservation を永続化してから実 PTY を一度だけ spawn し、output journal と terminal registry を
開始する。spawn failure・ambiguous・persist-after-spawn は fenced safe failure または reconcile-required
として保存し、replacement spawn を推測しない。Agent terminal の attach / input / resize / detach / exit は
[terminal ownership](#terminal-ownership) と同じ registry / stream contract を共有し、Agent owner と generic
owner を一つの shared terminal owner が `TerminalRef` の所有元へ routing する。connection close は当該
connection の subscription だけを外し、Agent process・PTY・completion worker は kill しない。

root の provisioner は Codex を既定 profile とし、`codex login status` または `claude auth status` を spawn
前に実行する。probe は executable の存在と製品が返す non-secret readiness/authentication status だけを判定し、
credential、token、設定 path、CLI 出力、OS error を保存・wire・UIへ渡さない。probe は composition root で
差し替え可能な境界であり、fixture executable を使う確認では実 CLI や実認証を必要としない。

### fixture による手動確認

root IPC の Agent fixture は次を確認する。実 CLI を install または login する必要はない。

1. `cargo test --test agent_ipc_e2e` を実行する。
2. 一時 Git repository、data directory、PATH 上の `codex` fixture に対して、root daemon の Unix IPC が omitted
   profile と explicit `codex` を受理することを確認する。
3. fixture が output、attach、input、detach、client disconnect、reattach、exit を通り、同じ operation の replay が
   completed terminal を返すことを確認する。
4. fixture を置かない場合と readiness status が失敗する場合に、PTY を spawn せず、install/sign-in を案内する
   safe `unavailable` だけを返すことを確認する。

pending Agent pane を attachable にするのは、同じ `OperationId` の成功 final が返す完全な `TerminalRef`
だけである。late / duplicate / wrong-generation / wrong-scope の completion は現 incarnation を変更しない。
TUI 側の pending pane と fenced attach policy は [3. TUI](03-tui.md) を正本とする。

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
