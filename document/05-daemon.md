# 5. daemon

> [ドキュメント目次](README.md) ｜ ← 前へ [4. daemon IPC](04-ipc.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

managed session と terminal を所有する daemon の現在の契約である。IPC wire と transport は
[4. daemon IPC](04-ipc.md) を正本とする。

## 目次

- [authority と lifecycle](#authority-と-lifecycle)
- [session tree と ignore rules](#session-tree-と-ignore-rules)
- [daemon process lifecycle](#daemon-process-lifecycle)
- [launchd supervision](#launchd-supervision)
- [daemon data directory](#daemon-data-directory)
- [failure logging](#failure-logging)
- [durable operation](#durable-operation)
- [terminal ownership](#terminal-ownership)
- [terminal launch environment](#terminal-launch-environment)
- [agent ownership](#agent-ownership)
- [supervisor scheduler](#supervisor-scheduler)
- [supervisor policy and verification](#supervisor-policy-and-verification)
- [generation と orphan safety](#generation-と-orphan-safety)
- [metrics observer](#metrics-observer)

## authority と lifecycle

managed session の lifecycle vocabulary は daemon のために定義されている。CLI、MCP、TUI は command を
提出し、legacy `state.json` を managed state として解釈・更新しない。shared lifecycle state の初期化時だけは、daemon が
legacy record の name、canonical session path、linked worktree、repository と `usagi/<name>` branch binding を全件検証し、
成功した全 record を stable ID 付き available session として一回だけ採用する。検証不能な record は partial adoption をせず起動を失敗させる。
lifecycle state は `creating`、
`initializing`、`available`、`deleting`、`failed` の closed vocabulary であり、Agent phase と branch
status は別軸として保持する。

durable reducer と store は accepted event ごとに `state_revision` を増やし、create/remove を operation と
session incarnation で fence する。IPC の create/remove は daemon が reservation を永続化してから worktree
effect を実行し、同じ daemon generation・operation・session attempt・revision の completion だけを反映する。
失敗した effect は safe failure として残り、client が local worktree 操作へ fallback しない。

各 managed session は `SessionId` と `WorktreeId` を同時に永続化する。agent / delegation が必要とする path は、available の workspace / session / worktree identity がすべて一致する場合だけ daemon が返す。creating、deleting、failed、stale identity、表示名・path-only の指定は scope に解決しない。

workspace root（`⌂ root`）も一つの scope として同じ仕組みで解決する。root scope は `session_id` を持たず（`None`）、workspace ごとに一度だけ生成して永続化した **root `WorktreeId`** で識別する。daemon は snapshot でこの root worktree id を公開し、launch 時に要求された workspace / root worktree identity が自分のものと一致する場合だけ、cwd を **trusted repository root** に解決する。root scope の cwd は常に daemon が持つ trusted root であり、client 供給の path は使わない。session scope の fence（`session_id` 必須の completion）はこの追加で回帰しない。詳細な設計根拠は [proposals/10-workspace-root-scope.md](proposals/10-workspace-root-scope.md)。

client に返す session 一覧は `available` の managed session だけを投影する。作成に失敗した reservation と中断後に reconcile された record は、operation の再送・復旧判断のため daemon の durable state に残すが、TUI の選択可能な一覧には出さない。

## session tree と ignore rules

`session create <name>` は workspace root が Git repository なら session root をその repository の
worktree にする。root が Git repository でない場合は、`.usagi/` と `.git` を除いて workspace を再帰的に
mirror する。走査中に見つけた各 Git repository は session tree 内の同じ相対 path に `usagi/<name>` branch
の worktree として作成し、plain file と directory は copy する。既存 linked worktree（`.git` が file）は
source に含めない。remove は mirror 内の worktree を子から順に Git で除去してから copied entries を除去する。

Git workspace を daemon が最初に開くと、`.usagi/.gitignore` に usagi 管理の ignore rules を書く。`issues/`
と `memory/` は共有・追跡対象のままにし、session tree、derived index、lock、その他の local metadata は
ignore する。旧版が repository root の `.gitignore` に書いた usagi 専用行は削除するが、他の行は保持する。

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
| `usagi daemon install-service` | macOS の LaunchAgent を明示的に install し、前景 `serve` を login と異常終了後に supervise する |
| `usagi daemon uninstall-service` | install 済み LaunchAgent を unload して remove する |

`serve` は process lifetime にわたって単一インスタンス lock を保持する。record は daemon の発見に使う
補助情報であり、単一インスタンスの権威は lock である。

IPC endpoint は `serve` が lock を取得して pid record を登録した後に、明示的な ready hook からだけ公開する。
lock を取得できない replacement は ready hook に到達しないため session request を受理できず、endpoint 公開に
失敗した daemon は pid record を消去して終了する。接続済み client は publish 済み generation の lifecycle
snapshot と operation ID を使って再接続する。再送された create は durable operation journal で照合し、worktree
effect を二重に実行しない。

## launchd supervision

macOS では `usagi daemon install-service` が `~/Library/LaunchAgents/com.usagi.daemon.plist`
を install する。LaunchAgent は絶対 path の `usagi daemon serve` だけを起動し、`RunAtLoad` と
`KeepAlive` により login・再起動・異常終了後に daemon process を起動する。plist に環境変数、token、
session state は保存しない。

launchd は process supervisor であり、managed session や Agent の権威を持たない。手動の `start` と
LaunchAgent が競合しても、`serve` が保持する `daemon.lock` が単一インスタンスを決める。lock を取得できない
process は IPC endpoint を公開しない。`uninstall-service` は supervision を止めるが、実行中 daemon の
停止は `usagi daemon stop` が担う。非 macOS では service 操作は unsupported として失敗し、既存の detached
`start` 経路は変わらない。

## daemon data directory

daemon の process lifecycle と Unix transport は `<data-dir>/daemon/` を使う。これは daemon の
内部状態であり、利用者が編集する設定ファイルではない。

| path | 種別 | 用途 |
|---|---|---|
| `daemon.json` | JSON | 稼働中 daemon の pid と登録時刻。daemon は起動時に書き、正常終了時に消去する |
| `daemon.lock` | lock file | `serve` が保持する単一インスタンス lock。process 終了時に OS が解放する |
| `current.json` | JSON locator | active daemon generation の Unix socket endpoint を atomically 公開する |
| `generations/<generation>/sock` | Unix domain socket | generation ごとの IPC endpoint。socket と locator は所有者・permission・symlink を検証して利用する |
| `sessions.json` | JSON | managed session の lifecycle、operation journal、stable identity と trusted repository root。daemon restart をまたいで共有する |
| `terminals.json` | durable atomic JSON | generic terminal の launch reservation、trusted profile provenance、process identity、runtime state。PTY master と output journal は process memory にのみ保持する |
| `agents.json` | durable atomic JSON | Agent runtime の launch reservation、semantic operation key、safe outcome、public launch plan snapshot、process identity、runtime state。argv や secret を含む adapter private provision と PTY master は永続化しない |
| `dispatch.json` | durable atomic JSON | dispatchable agent、dispatch run、caller↔worker binding のレジストリ。run ID は既存の durable `OperationId` を使う |
| `inbox/<caller-session-id>/<caller-agent-id>.jsonl` | durable atomic JSONL | caller agent 単位の完了報告 inbox。cross-process lock 下で更新するため、caller の停止中にも報告を保持する |

`data_dir` は release では `$USAGI_HOME` または `~/.usagi`、debug（`cargo run` を含む）ではその `dev/` 子 directory である。プロジェクト内の debug runtime state も同じ定義を使い、`<project_root>/.usagi/dev/` に保存する。旧 `develop/` / `development/` との互換処理は行わない。
したがって `cargo run` は production の record / locator / lock / daemon-owned state に触れず、
`cargo run --release` は従来の production channel を使う。`USAGI_HOME` を明示しても同じ分離を適用する。

managed session state は repository 内の `.usagi/` ではなく、この shared daemon directory に保存する。最初の
起動時だけ従来の `<repository>/.usagi/lifecycle-state.json` があれば `sessions.json` へ atomically 移行して削除する。lifecycle
state が無い場合は、検証済みの project runtime state（debug は `<repository>/.usagi/dev/state.json`）の session も available record として同じ atomic write で採用する。
この adoption は worktree effect を実行せず、既存 `sessions.json` があれば legacy state を読まず、その durable state を変更しない。
`state.json` に残る display name、origin、notes、PR、last-active は UI-only metadata であり、TUI は同名 managed session へ読み取り結合する。
以後の restart は起動 cwd に関係なく、同じ file に保存された trusted root を session runtime と generic terminal の `login-shell` profile の両方に使う。

既存の `sessions.json` に legacy session を追加する必要がある場合だけ、operator は `usagi session recover-legacy` を実行する。これは dry-run で candidate 名と検証結果だけを表示し、`--apply` を付けた明示操作だけが adoption を永続化する。daemon restart、TUI sidebar refresh、通常の MCP session tool は recovery を呼ばない。MCP の `session_recover_legacy` も同じく `apply: true` がなければ dry-run である。

apply は legacy record 全件の name、期待 path、linked worktree、canonical path、`git worktree list --porcelain` の `usagi/<name>` branch binding を検証する。legacy 内の重複、欠損・不正 record、Git 検証失敗、既存 v2 session との同名（available / creating / deleting / failed を問わない）、または revision 競合は fail-closed となり、`sessions.json` を変更しない。成功時は既存 v2 record と stable ID を保持したまま、検証済み全 record を fresh stable IDs の available session として単一 atomic write で追加する。legacy UI metadata は read-only のままである。

`daemon.json` は `pid` と `started_at` を持つ。`current.json` は generation、daemon directory からの
相対 endpoint、`active` または `draining` の state を持つ。socket endpoint は永続データではなく、
daemon generation の終了とともに消える。

`terminals.json` と `agents.json` は source-of-truth snapshot として、writer ごとの一意 temporary file に
書き込み・fsync した後に rename で置換する。rename 後は対応可能な platform で parent directory も fsync
するため、途中の snapshot を公開せず、電源断後にも rename を永続化する。保存に失敗した場合は既存の
snapshot を置換せず、失敗した temporary file を削除する。

daemon restart 時は `agents.json` と `terminals.json` を spawn admission より前に読む。Agent runtime は
coordinator、semantic operation ledger、safe outcome を hydrate する。両 snapshot の未終端 runtime は
`identity_unknown` の reconcile state に atomic に移し、Agent operation は `ownership_unknown` outcome とする。
死んだ daemon の PTY master は復元不能であり、PID だけでは child の所有権を証明できないため、この遷移は
attach、input、resize、kill、replacement spawn を行わない。runtime は元の workspace / session / worktree /
daemon generation / operation fence を保持したまま inventory に `live: false` として投影する。`exited` runtime
はそのまま残り、Agent の success / non-zero-exit outcome は同じ意味で replay する。

旧 snapshot は次の launch による保存でも削除しない。snapshot の JSON 破損、未知 schema、重複 operation、
scope / generation / operation fence の不整合、または reconcile write failure は daemon startup を fail closed
にする。この状態では runtime を公開せず、spawn も snapshot の上書きも行わない。reconcile 件数と store failure
は日次 error log に記録され、session lifecycle vocabulary は変更しない。旧 Agent schema は欠けた semantic key
や outcome から成功を捏造せず、該当 operation を `identity_unknown` の非 spawnable safe failure として現 schema
に移行する。credential は hydrate せず、restart 後も ephemeral に失効する。旧 PTY 自体は resume せず、利用者には
inventory の `live: false` と typed safe error で非 live を明示する。

## failure logging

daemon の最外周は返却された IO error を捕捉して `<data-dir>/logs/error-YYYY-MM-DD.log` に記録する。IPC、PTY、
observer など daemon worker thread の panic は process-wide panic hook が payload、発生位置、backtrace とともに
同じログへ記録する。main thread の panic はこの hook で記録した後に最外周で通常の process error に変換して終了する。
これにより detached `serve` の標準エラーが破棄される場合でも、起動失敗や異常終了の原因を日次 error log から確認できる。

## durable operation

operation journal は operation ID、owner daemon generation、execution attempt、progress revision、status
を保存する。status は `accepted`、`running`、`cancel_requested`、`succeeded`、`failed`、`cancelled`、
`ambiguous` である。terminal status になった operation を同じ ID で restart しない。

durable store は、受理される create / remove operation の owner generation が daemon と一致することを
検証する。completion は `CompletionFence` と reducer transition の両方を満たす場合だけ反映される。
このため ACK loss や late worker で effect の結果を推測して二重実行しない。

daemon 起動時には未完了の create / initialize / delete journal を reconcile する。physical effect の完了を証明できない record は再実行せず safe failure にして明示 recovery を待つ。

interrupted reconciliation は session を `failed`、対応 operation を terminal `failed` に同じ durable state で記録する。元の `OperationId` の再送は保存済み safe failure を返し、effect を再試行しない。operator が filesystem / Git の状態を確認・修復した後は、明示 recovery または新しい `OperationId` による許可された lifecycle 操作を使う。

旧 reducer が書いた `session.lifecycle = failed` と `operation.status = succeeded` の矛盾した snapshot は daemon open 時に保守的に補正する。failure stage、session name、operation の canonical semantic key が一致する operation だけを `failed` に戻して関連付け、成功 outcome や success hook は生成しない。この移行は effect の再実行可能性を推測しないため、自動 retry は行わず明示 recovery を待つ。

## terminal ownership

terminal registry は daemon generation が所有する `TerminalRef` を key にする。attach は snapshot と
subscription を atomically 作り、detach と client disconnect は当該 connection の attachment だけを
外す。PTY、output journal、process ownership は client disconnect では解放しない。

raw output は terminal ごとに最大 64 KiB の bounded retention window として offset を付けて保持する。
連続 output は一つの retained segment に coalesce し、上限超過時は古い prefix を byte 単位で trim
するため、byte 数だけでなく segment metadata も bounded である。attach client は snapshot の
`base_offset` と `output_offset` を検証した後、連続する output offset を適用する。journal に残らない
cursor、sequence gap、epoch mismatch は resync を要求する。
terminal input は `(ClientId, TerminalRef, input sequence, RequestId)` で dedupe し、同じ input batch を
別 connection から重複 write しない。input は queue capacity を予約してから enqueue し、ACK は全 byte が
PTY endpoint に書き込まれた後だけ返す。partial write は ambiguous として扱う。

PTY reader は最大 4 KiB の output item を、Agent と generic terminal それぞれ容量 64 item の
bounded observation queue に送る。queue full では reader thread だけを backpressure し、registry owner、
別 client の IPC thread、別 terminal の reader に unbounded allocation を発生させない。queue は FIFO で
output を drop せず、reader が EOF 後に exit を同じ queue へ送るため、最終 output の後にだけ exit が
commit される。slow / absent client は polling cursor を持つだけで reader queue の consumer ではない。

terminal resize は registry の revision と geometry を更新する。terminal exit は final output を append
してから exited state を記録するため、ownership を early release しない。reader が child を reap し、owner が
exit を registry と durable runtime record へ commit した後（exit 後の store write が失敗した場合は
`persist_after_exit` を記録した後）、process-local transport map から完全な `TerminalRef` で fenced entry を
exactly once で外す。これにより PTY master、writer、reader と child handle の FD は attachment の有無にかかわらず
解放される。exit と競合または exit 後に届いた input / resize は別 incarnation の PTY へ fallback せず
`stale_target`、duplicate exit は同じ typed failure となる。detach は subscription だけを冪等に処理し、transport
entry の寿命を延ばさない。

durable terminal record と bounded output journal は transport entry とは別の tombstone state である。exit 後も
最終 status、offset、最大 64 KiB の replay window を attach / resume / resync に返す一方、PTY handle や FD は
保持しない。したがって final replay の保持量は terminal 数に対する byte bound の範囲内であり、transport の
回収を妨げない。

generic shell terminal は root IPC server が全 connection で共有する ownership runtime へ渡す。runtime は
generic terminal coordinator、trusted `login-shell` profile resolver、durable terminal store、実 PTY adapter
を一つの ownership loop に保持する。PTY reader は output journal へ drain され、connection close は runtime
に通知して当該 connection の subscription だけを外し、profile resolution や replacement spawn を行わない。

## terminal launch environment

`login-shell` は daemon 起動時に読み取った public terminal environment から、絶対 path の `SHELL` を
program として選ぶ。存在しない、相対 path、または NUL を含む値は `/bin/sh` へ fallback する。PTY 上では
`-l -i` を渡し、shell の login と interactive startup を有効にする。daemon は client の完全な
workspace / session / worktree ID を `SessionRuntime` の available managed session と照合してから、その同じ
worktree path を cwd として profile resolver に渡す。`session_id` を持たない **root scope** は、
workspace と root worktree identity を daemon の永続 state と照合してから cwd を trusted repository root に
解決する。いずれの scope でも不一致・unavailable なら spawn 前に拒否されるため、`TerminalLaunchRequest` の
scope と実際の cwd は常に同じ scope（managed session または workspace root）を指す。IPC client が任意の
path・argv・environment・root worktree identity を指定することはできない。

| 項目 | 扱い |
|---|---|
| `SHELL` | 起動 program の選択に使い、child environment にも引き継ぐ |
| `TERM` | 親 terminal の値を引き継ぎ、PTY の terminal capability を上書きしない |
| `PATH` / `HOME` | 親 terminal の command search path と home directory を引き継ぎ、login startup が追加する設定を妨げない |
| `LANG` / `LC_ALL` / `LC_CTYPE` | UTF-8 と wide character の locale を引き継ぐ |
| `COLORTERM` / `COLORFGBG` / `NO_COLOR` | 色深度・背景色・色無効化の terminal 設定を引き継ぐ |
| `TERM_PROGRAM` / `TERM_PROGRAM_VERSION` | macOS Terminal などの terminal 固有設定を引き継ぐ |
| `TERM_SESSION_ID` | child では空にして、Terminal.app 固有の session 保存・復元を無効化する |
| `ZDOTDIR` / `XDG_CONFIG_HOME` | shell の user configuration の位置を引き継ぐ |
| その他・secret | profile resolution は収集・保存・転送せず、PTY child は daemon の ambient environment から継承しない |

実 PTY の spawn 境界は親 environment を必ず clear し、次の許可済み live source だけから child environment を
再構築する。この表が供給元と同名 key の優先順の正本であり、下の source ほど優先する。

| 優先順 | 供給元 | 対象 |
|---|---|---|
| 1 | public terminal profile | 上表の非 secret 変数。generic terminal と Agent の共通基底 |
| 2 | validated Agent adapter provision | adapter が型検証済みの変数名で spawn 時だけ供給する private 値。generic terminal には存在しない |
| 3 | daemon-issued ephemeral provision | live Agent runtime に結び付く caller credential。同名の adapter provision より優先する |

generic terminal と Agent は同じ PTY spawn 境界を通る。空の environment を渡した場合も ambient environment へ
fallback しない。environment の同名 key は上表の優先順で一つに畳み込んでから child に渡す。

durable terminal record には profile、program、public argv、working directory と environment **名**だけを保存する。
environment の値は PTY spawn 時だけに使い、record、IPC payload、output journal には含めない。PTY resize は
daemon-owned master に適用され、detach は subscription のみを外すため、macOS を含む Unix で shell の process
group、signal、resize と clipboard escape sequence を client process が横取りしない。

> **破壊的変更（release note）:** PTY child が allowlist 外の任意 daemon environment を暗黙に継承する挙動は
> 廃止される。その値に依存する shell / Agent 設定は動作しなくなる。durable snapshot と IPC wire の schema は
> 変わらないため、データ移行や wire migration は不要である。

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

### Agent admission transaction

Agent admission transaction の正本は本節である。daemon は一つの operation に対し、次の順序を崩さない。

```text
validate
  -> prepare dispatch reservation
  -> register ephemeral credential in memory
  -> prepare runtime reservation
  -> spawn once
  -> commit runtime + dispatch as Running
       | failure after spawn
       v
     terminate process group -> reap child -> persist Failed / reconcile-required
```

| phase | durable state | process / credential |
|---|---|---|
| prepare | operation、`Preparing` run、binding、`Starting` agent、terminal/runtime fence、semantic key、credential provenance を保存する | process は存在しない。daemon-minted secret を in-memory caller registry に登録する |
| spawn | prepare 済み operation だけが一度だけ PTY child を起動する | secret は spawn provision にだけ存在し、child の最初の MCP call より前に caller registry が利用可能である |
| commit | runtime process identity と `Running` run/agent を保存する | commit 完了後だけ admission success を返す |
| compensate | post-spawn の runtime/dispatch 保存失敗を safe failure または reconcile-required として保存する | exact terminal owner が process を terminate して reap する。terminate/reap を証明できなければ orphan-running として fail closed する |

同じ operation の retry は保存済み semantic key と outcome を replay し、異なる intent は
`idempotency_conflict` になる。`Preparing` / `Starting` は成功 outcome ではなく、daemon restart 時に
`Failed` / ownership unknown へ reconcile される。admission metadata を持たない legacy run、または runtime
ownership を証明できない incomplete record も新しい child を spawn せず、unknown/failed として扱う。

credential の durable form は `daemon_minted_ephemeral` という provenance だけである。opaque secret 自体は
dispatch registry、runtime snapshot、IPC、terminal journal、log のいずれにも保存しない。daemon restart では
in-memory caller registry が空になるため、旧 credential は必ず失効する。

restart 後の Agent owner は hydrate 済み operation を admission より先に照合する。同じ semantic intent は保存済み
accepted / completed / safe failure を replay し、同じ `OperationId` の異なる intent は
`idempotency_conflict` にする。新規 launch の snapshot write は hydrate 済み全 record を含むため、過去の terminal、
binding generation、outcome を脱落させない。

[`terminal inventory`](04-ipc.md#generic-terminal-request) request も shared terminal owner が処理し、
generic owner と Agent owner の両方に scope を問い合わせて結果を merge する。したがって列挙には generic
terminal と Agent terminal の両方が含まれ、各エントリは `TerminalRef`・`kind`・`live`（現 generation が所有し
attach 可能か）だけを持つ。これは client が workspace open 時に live runtime を pane へ復元するための source of
truth である（[3. TUI](03-tui.md#workspace-open-時の-pane-復元) を正本とする）。

Codex / Claude の Agent launch は `McpWiring` capability を要求し、daemon 自身の絶対パスで `usagi mcp` を
子 MCP server として起動する。製品ごとの MCP 設定は adapter provision が spawn 時だけに渡すため、設定 payload は
public launch plan、durable snapshot、IPC response に残らない。注入した usagi MCP tool は agent が確認なしで
呼べる。Codex は `approval_policy = never`（provision が渡す argv と `.codex/config.toml`）で MCP tool 呼び出しの
確認プロンプトを出さない。Claude は注入した `usagi` server のツールだけを事前許可する（`--allowedTools mcp__usagi`）
ため、他の MCP server・shell・ファイル編集・network の permission model は通常どおり維持され、無効化・緩和しない。

daemon が起動した MCP child だけには、live Agent runtime に結び付く opaque な caller credential を
private provision として渡す。`user_decision_*` はこの credential、daemon generation、terminal incarnation、
dispatch binding を照合して owner を再構成し、workspace root は `session_id: None` の root scope として保存する。
手動起動した `usagi mcp`、credential の欠落・偽造・失効、または stale runtime は owner を推測せず
`ownership_unknown` で拒否し、decision state を変更しない。credential と private provision は durable snapshot、
IPC、TUI、log に保存・公開しない。
この事前許可も spawn 時 argv に限り、durable snapshot や IPC response には残らない。

[`dispatch` request](04-ipc.md#dispatch-request) はこの launch 経路を再実装せずに合成する。daemon は session を lifecycle 経由で upsert し、worker Agent と `DispatchRun` / caller↔worker binding を durable registry に保存してから同じ runtime で prompt を起動する。PTY exit の durable commit 後、Completed / Failed inbox delivery が無ければ caller inbox に NoReport を一度だけ配送する。completion と exit は同じ `CompletionFence` を照合するため、late、duplicate、wrong-generation は state や inbox を変更しない。

新規 worker の runtime/model は MCP schema snapshot を信頼せず、spawn の直前に resolved managed-session worktree の current `.usagi/config.toml` allowlist と current executable locator で再検証する。allowlist 外・不完全な runtime/model は safe `invalid_argument`、CLI 不在は safe `unavailable` となり、reservation や spawn を行わない。既存 `agent.id` はこの再選択を通らず、保存済み agent の session ownership と lifecycle scope をそのまま用いる。allowlist、executable、または MCP wire / durable registry に path、argv、environment、credential、raw CLI output、provider model list は保存しない。

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

## supervisor scheduler

daemon は connection ごとではなく一つの `SupervisorRuntime` を所有する。completion、failure、
NoReport、起動時 reconcile、明示 wake は対象 run の有限な tick を起動し、idle 時に poll しない。

tick は dispatch run ID と supervisor provenance を照合して terminal fact を reducer event として保存する。
child terminal 後に parent が `Running` なら `AwaitingDecision` に遷移し、parent provenance と child run、
safe completion summary、DAG state、decision generation を含む wake reservation を durable に保存してから
parent wake effect を実行する。reservation は child run と parent generation で一意なので、duplicate event、
ACK loss、daemon restart は同じ wake を二重に作らない。parent runtime の再解決・restart は wake adapter が
保存済み provenance だけを使って行い、session 名から target を推測しない。

## supervisor policy and verification

各 `SupervisorRun` は作成時の immutable `ExecutionPolicy` snapshot を durable state に保存する。現在の既定値は dispatch 16 回、同時実行 4、親子深さ 8、retry attempt 1（fail-closed）、retry backoff 30 秒である。request ごとの上限緩和は受け取らない。

`Dispatch` reducer event は policy admission と同時に dispatch reservation を保存する。dispatch budget、concurrency、depth のいずれかを超える event は worker effect へ進まず、safe evidence と resume/cancel の選択肢を持つ durable `EscalationRecord` を保存して run を `Escalated` にする。escalation を scheduler が自律的に解除することはない。

failure は policy の attempt 上限内だけ `Retrying` へ遷移し、generation と `retry_at` を保存する。scheduler は deadline 後にだけ `RetryReady` event を保存するため、restart や duplicate completion は retry を早めない。run/task の cancel event は未完了 node を `Cancelled` にし、terminal run への late completion は reducer が拒否する。

artifact contract が `none` 以外の worker completion は `Succeeded` ではなく `Verifying` へ遷移する。独立 verifier が保存した digest を伴う `VerificationResult` が passed の場合だけ `Succeeded` となる。failed verification は escalation record を保存するため、worker の summary や PR URL 単独では success gate を通らない。

## metrics observer

daemon は metrics observer ごとに容量 1 の bounded queue を持ち、periodic tick の snapshot を
non-blocking に fan-out する。observer が遅い場合は中間 snapshot を coalesce して drop count を
増やし、切断された observer は次の tick で外す。これは観測用経路なので、session reducer、PTY、
Agent runtime の進行を block しない。登録、解除、再接続時の protocol は
[4. daemon IPC](04-ipc.md#daemon-metrics-subscription) を正本とする。

同じ snapshot は terminal retention で trim / coalesce した byte 数と、PTY observation queue で
backpressure した byte 数も process-local counter として返す。counter と log は byte 数だけを扱い、
terminal output、argv、environment、secret を含めない。

TUI は最新 snapshot を workspace の左ペイン下部にある v1 互換の usagi mascot の足元の右へ表示する。
この観測値は操作対象ではないため、狭い terminal では session 一覧と footer を優先して mascot ごと省略される。

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
