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
- [PR refresh scheduler](#pr-refresh-scheduler)
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

create/remove の重い Git worktree 構築・撤去（`git worktree add` / `remove`）は**共有 session lock を解放した状態で実行する**。lock を握るのは fast な durable transition（reservation・`BeginRemove`・completion の永続化）だけであり、その間に session 一覧・terminal poll・user-decision 一覧など他 connection の read が同じ lock 待ちで固まらない。したがって長い worktree 操作の最中も daemon は応答し続け、TUI の描画・入力ループが session 作成・削除で凍結しない。

各 managed session は `SessionId` と `WorktreeId` を同時に永続化する。agent / delegation が必要とする path は、available の workspace / session / worktree identity がすべて一致する場合だけ daemon が返す。creating、deleting、failed、stale identity、表示名・path-only の指定は scope に解決しない。

workspace root（`⌂ root`）も一つの scope として同じ仕組みで解決する。root scope は `session_id` を持たず（`None`）、workspace ごとに一度だけ生成して永続化した **root `WorktreeId`** で識別する。daemon は snapshot でこの root worktree id を公開し、launch 時に要求された workspace / root worktree identity が自分のものと一致する場合だけ、cwd を **trusted repository root** に解決する。root scope の cwd は常に daemon が持つ trusted root であり、client 供給の path は使わない。session scope の fence（`session_id` 必須の completion）はこの追加で回帰しない。詳細な設計根拠は [proposals/10-workspace-root-scope.md](proposals/10-workspace-root-scope.md)。

client に返す session 一覧は、使用可能な `available` に加えて、名前を占有し続ける `failed`（作成に失敗した reservation と中断後に reconcile された record）も lifecycle と失敗理由付きで投影する。過渡状態（`creating` / `initializing` / `deleting`）は一覧に出さない。各行の可否（attach / remove など）は wire に載る lifecycle から client 側で導出する（`SessionLifecycle::capabilities` が正本）。`failed` 行は使用不可（attach を提示しない）だが削除可能で、削除すると worktree 未作成でも名前が解放されて同名 create が再び通る。一覧への投影は attach 対象を広げない: scope 解決は引き続き `available` だけを対象とする（前述）。

## session tree と ignore rules

`session create <name>` は lifecycle の reservation と Git effect の前に `.usagi/sessions/<name>` の
存在を検査する。snapshot に未登録の stale directory や dangling symlink も占有済みとして拒否する。
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
CLI operation、MCP server は共有 bootstrap を通る。release / distributed / development / local の全 channel は、
startup 時に確定した canonical build artifact identity が exact match の active endpoint を再利用する。identity は
build schema、profile、full target、source tree / compiler / feature / rustflags identity を含み、version / target だけの一致や
literal `"unknown"` を same build と扱わない。Git metadata の無い package build も package source set で
識別し、identity read failure は unknown として fail safe にする。

build mismatch の通常 bootstrap は artifact pair と channel から決まる stable operation ID の typed rollover trigger を
生成する。production / local は old daemon を停止せず、trigger を返して old endpoint と live PTY をそのまま維持する。
development は trigger を cold restart で消費し、replacement の exact artifact を handshake で確認してから再接続する。
これにより `USAGI_RUNTIME_MODE=development cargo run` は再コンパイル後も起動できるが、old daemon が所有する live
Agent / generic Terminal は継続しない。同じ artifact の通常 TUI / CLI / MCP 起動は trigger 0 で daemon を再利用する。intentional な
same-artifact replacement は通常 bootstrap と分離した `usagi daemon replace` が force trigger を発行する。trigger は
effect-free であり、production / local は cross-process standby / admission consumer が未接続の間は cold stop/start や
二重 spawn に進まない。
unknown identity、`build.artifact.v1` capability の無い old daemon、read / verification failure も old daemon を維持した
typed refusal になる。

したがって TUI の終了や同 build client の再接続だけでは daemon-owned Agent PTY は失われない。一方、明示的な cold
`daemon restart` は旧 owner process を終了するため、その process が持つ PTY master と
live Agent / generic Terminal を継続できない。fresh daemon は unfinished runtime を `identity_unknown` へ reconcile し、旧
`TerminalRef` を live として復元しない。この production gap は
[#507](../.usagi/issues/507-fix-daemon-planned-restart-active-draining-generation-rollover.md) で追跡する。前提となる build
artifact identity / safe trigger は
[#528](../.usagi/issues/528-fix-daemon-build-artifact-identity-safe-rollover-trigger.md)、cross-process authority は
[#516](../.usagi/issues/516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)、owner runtime
の永続化と handoff は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md)、shipping enable 前の
owner-generation routing は
[#508](../.usagi/issues/508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) に分割する。#507 は #508
完了後だけ rollover を有効化する。検証済み active locator への接続が `ConnectionRefused` になった場合だけ、共有
bootstrap は後述の exact stale-owner recovery を試みる。draining、malformed / unsafe locator、所有権または lifecycle
record の安全性が不明な場合は replacement を起動せず、安全な typed lifecycle error を表示する。client が
daemon-owned terminal や managed session をローカルに代替実行することはない。

```text
same build reconnect
  client -> current.json -> existing daemon process -> existing PTY master

detected build mismatch / daemon replace
  client -> stable rollover operation -> typed trigger, stop effect 0
                                  -> old daemon process + PTY remain alive

development build mismatch
  client -> stable rollover operation -> cold restart -> exact build reconnect

manual cold restart
  client -> stop old -> quiesce / retire endpoint -> old process exit
                                                    -> PTY master is not transferred
         -> start fresh -> reconcile unfinished = identity_unknown
                        -> publish new current.json -> reconnect
```

daemon verb を含む process argv は、合成ルートが side effect より前に完全に解析する。本節の表は
解析成功後の lifecycle effect を定義し、文法・usage error・終了 status は
[2. アーキテクチャの process argv contract](02-architecture.md#process-argv-contract) を正本とする。

| コマンド | 動作 |
|---|---|
| `usagi daemon start` | detached `serve` を起動し、`daemon.json` に稼働中の pid が登録されるまで待つ。すでに稼働中なら新しい process を起動しない |
| `usagi daemon status` | lifecycle record と exact process-start identity の観測から running / stale / unverified / absent を表示する |
| `usagi daemon stop` | exact owner の稼働中 daemon に終了を要求し、endpoint cleanup の完了後に lifecycle record を消去する。stale record は process に signal を送らず、singleton lock 下で stale endpoint を回収してから消去する。unverified record は signal・回収とも拒否する |
| `usagi daemon restart` | 稼働中 daemon を停止してから新しい daemon を起動する。active / draining handoff は行わない |
| `usagi daemon replace` | exact artifact の意図的な replacement trigger を要求する。同じ artifact pair / channel は同じ operation ID へ収束し、この command 自体は old daemon を停止しない |
| `usagi daemon` / `usagi daemon serve` | 前景で daemon を serve する。`serve` は内部用の subcommand である |
| `usagi daemon install-service` | macOS の LaunchAgent を明示的に install し、前景 `serve` を login と異常終了後に supervise する |
| `usagi daemon uninstall-service` | install 済み LaunchAgent を unload して remove する |

`serve` は process lifetime にわたって単一インスタンス lock を保持する。record は daemon の発見と exact owner
確認に使い、単一インスタンスの権威は lock である。record の owner identity は PID と OS が返す process-start
identity の組であり、macOS では process start time、Linux では `/proc/<pid>/stat` の start time を opaque token
として保存する。

IPC endpoint は `serve` が lock を取得して exact process-owner record を登録した後に、明示的な ready hook からだけ公開する。
lock を取得できない replacement は ready hook に到達しないため session request を受理できず、endpoint 公開に
失敗した daemon は endpoint cleanup の完了を証明できた場合だけ exact owner record を消去して終了する。接続済み client は publish 済み generation の lifecycle
snapshot と operation ID を使って再接続する。再送された create は durable operation journal で照合し、worktree
effect を二重に実行しない。

`serve` は singleton lock 取得後、旧 lifecycle record を snapshot し、record の有無にかかわらず stale endpoint recovery を
新 record の保存より先に完了する。recovery 後に record が snapshot と exact 一致することを再確認し、その場合だけ新 incarnation へ
atomic save して publish する。recovery failure または concurrent record replacement では旧 record / replacement を上書きせず、
新 endpoint も公開しない。この pre-registration fence は ordinary `start` と LaunchAgent による直接 `serve` の双方に適用される。

endpoint bind 後の startup failure では、listener fd と独立した exact generation cleanup token を保持する。
accept-loop panic や join / retire failure で worker または listener を失っても token を再試行し、socket と locator の
不在を証明できた場合だけ lifecycle record を消去する。bind 自体が token 構築前に失敗した場合も、singleton lock を保持する
owner が private generation nodes を検証・回収する。cleanup proof を得られない場合は record を completion fence として
残し、後続の stale `stop` に同じ安全な recovery を委ねる。

accept worker は exit guard を持ち、panic または unexpected exit で shared shutdown fence を立てる。main shutdown wait は
OS signal と同じ fence を監視するため、worker を失ったまま singleton lock と record を永久保持しない。main は wake 後に
join failure を観測しても独立 cleanup token から retirement を試み、cleanup 成功後だけ record を消去する。

`serve` は endpoint 公開と worker spawn より前に SIGINT / SIGTERM handler と同期 wait を準備する。handler は signal
受理時点で shared shutdown flag を立てるため、重い endpoint 初期化中に停止要求が届いても、その後に起動する accept loop は
新規 connection を受理しない。handler は process の signal mask を変更せず、その後に起動する child process へ blocked signal を
継承させない。planned stop では signal を受けた owner が次の順序を `daemon.lock` の保持中に完了する。endpoint の
generation fence と unlink 規則は [4. IPC#Unix transport](04-ipc.md#unix-transport) が正本である。

```text
shutdown signal
  -> shared shutdown flag を設定
  -> 新規 connection accept を停止
  -> accept loop を join（listener ownership を回収）
  -> 既接続 client worker は shutdown / join しない（現行制約）
  -> owner generation の socket / current locator をこの順に retire
  -> 自 incarnation と一致する場合だけ daemon.json を消去
  -> daemon.lock を process exit で解放
```

`daemon.json` は endpoint retirement の completion fence である。`status` / `start` / `stop` は record の PID に現在
存在する process の start identity が保存値と一致する場合だけ owner を alive と扱う。PID が存在しない場合だけ stale
として reclaim でき、PID reuse、identity 欠落、OS observation failure は `unverified` として record を保持する。
`stop` は signal 直前にも identity を再検証し、Linux では identity 確認済みの pidfd に SIGTERM を送る。macOS では
`proc_pidinfo` で process start time を直前に再検証してから SIGTERM を送る。legacy record、mismatch、unknown identity
へ raw PID signal を送らない。

running `stop` は SIGTERM を送っても先行消去せず、
owner が retire 成功後に exact record を変更・消去するまで有界に poll する。PID が消えても同じ record が残る場合や
shutdown window を超えた場合は cleanup failure として record を保持するため、stale locator のまま replacement を
起動しない。locator が先に `NotFound` となる短い区間でも live record と `daemon.lock` が replacement 起動を抑止し、
`stop` は最後の record clear まで成功を返さない。

stale `stop` は scoped `daemon.lock` を取得し、lock 下で最初の lifecycle record 全体がまだ exact current record であることを
再確認する。その後 `current.lock` 下で current が指す socket と安全に検証できる orphan socket を先に回収し、exact locator、
exact record の順に消去する。socket removal failure では locator を、locator cleanup failure では record を残すため、各 crash
point は次回 `stop` で再試行できる。locator が既に無い場合も private generation directory 内に安全に回収できない socket node が
無いことを証明してから record を消去する。lock 取得前後に replacement record が保存された場合は何も消去せず fail closed とし、
replacement の record、locator、socket を保持する。

ordinary TUI / CLI bootstrap で検証済み active locator への接続が `ConnectionRefused` になった場合は、手動の
`stop` / `start` を要求せず、次の順序で同じ stale cleanup を一度だけ試みる。この順序では connection error 自体を
stale 判定に使わない。

```text
bootstrap.lock を保持
  -> lifecycle record 全体を snapshot
  -> daemon.lock の取得を試行
  -> lock 下で record 全体を exact recheck
  -> current.lock 下で socket / current locator をこの順に cleanup
  -> record.lock 下で同じ exact record だけを conditional clear
  -> daemon.lock を解放して replacement を起動し、readiness を確認
```

`daemon.lock` の取得が active usagi owner の不在証明である。lock が busy なら live / starting owner を優先し、record、
socket、locator を変更せず接続を retry する。lock を取得できても、record replacement または cleanup failure では
同様に既存状態を保持して fail closed とする。raw PID の signal-0 結果や `ConnectionRefused` 単独では process の
incarnation と所有権を証明できないため cleanup authority にならず、この recovery は process へ signal を送らない。
`stop` と owner cleanup は最初に読んだ lifecycle record 全体
`(pid, process_start_identity, started_at)` を最後まで保持する。`daemon.json` の save と
conditional clear は private な `record.lock` の同じ cross-process transaction で直列化し、比較と unlink の間に
replacement save が割り込む隙間を作らない。同じ pid が再利用されても process-start identity または `started_at` が
異なる replacement record は
old stop、stale reclaim、遅延 owner cleanup のいずれからも保護される。save は同じ directory の private unique
temporary を完全に write / fsync してから atomic rename し、conditional unlink とともに parent directory も fsync する。
parent directory fsync は platform / filesystem が対応する範囲の best-effort とし、commit 済み rename / unlink を
事後 error として曖昧に報告しない。途中の write、crash、rename failure は既存の正常な record を空または partial JSON に
置換しない。呼出元へ write / rename error を返す経路は temporary の unlink を試み、その cleanup failure も error として返す。
hard crash では unique temporary が残り得るが、後続 save は別名を使うため阻害されない。

この順序は新規 connection を止めるが、accept 済み connection の frame dispatch、reserve/spawn/control effect を
停止しない。client worker の JoinHandle も保持しないため、role 付き request lease、internal producer の停止、既接続
stream の shutdown/join は未実装である。この admission race は
[#516](../.usagi/issues/516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) で追跡する。
正常終了後の discovery は stale socket への `ConnectionRefused` ではなく `NotFound` になる。client bootstrap は
locator 自体の `NotFound` では replacement を一度起動する。検証済み locator の endpoint 検証または connect 後の
`NotFound` は `ConnectionRefused` 相当に分類し、上記の fenced recovery が完了した場合だけ起動する。その他の接続失敗、
draining、不正 endpoint では replacement を起動せず fail closed にする。

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
| `daemon.json` | JSON | 稼働中 daemon の pid と登録時刻を持つ lifecycle record。daemon は起動時に書き、endpoint cleanup 後に exact record だけを消去する |
| `daemon.lock` | lock file | `serve` が保持する単一インスタンス lock。process 終了時に OS が解放する |
| `bootstrap.lock` | lock file | client の connect/start/restart/recover bootstrap を cross-process で直列化する |
| `record.lock` | lock file | `daemon.json` の read、save、incarnation-conditional clear を cross-process で直列化する |
| `current.lock` | lock file | current locator の publish と generation-fenced retire を cross-process で直列化する |
| `current.json` | private atomic JSON locator | active daemon generation の Unix socket endpoint を公開する。安全な publication の正本は [4. IPC の Unix transport](04-ipc.md#unix-transport) |
| `generations/<generation>/sock` | Unix domain socket | generation ごとの IPC endpoint。socket と locator は所有者・permission・symlink を検証して利用する |
| `sessions.json` | JSON | managed session の lifecycle、operation journal、stable identity と trusted repository root。daemon restart をまたいで共有する |
| `terminals.json` | durable atomic JSON | generic terminal の launch reservation、trusted profile provenance、process identity、runtime state。PTY master と output journal は process memory にのみ保持する |
| `agents.json` | durable atomic JSON | Agent generation/terminal ownership と runtime の launch reservation、semantic operation key、safe outcome、public launch plan snapshot、process identity、runtime state、`AgentContinuationRef` / source relation、最小の `ProviderResumeRef`。ownership と runtime record は一つの snapshot で遷移する。provider ID は sensitive metadata とし、argv や secret を含む adapter private provision、PTY master、transcript は永続化しない |
| `pr-inventory.json` | durable atomic JSON | session ごとの canonical PR、last-known title/state、user-owned pin/dismiss、safe refresh state |
| `dispatch.json` | durable atomic JSON | dispatchable agent、dispatch run、caller↔worker binding のレジストリ。run ID は既存の durable `OperationId` を使う |
| `inbox/<caller-session-id>/<caller-agent-id>.jsonl` | durable atomic JSONL | caller agent 単位の完了報告 inbox。cross-process lock 下で更新するため、caller の停止中にも報告を保持する |

runtime mode は `USAGI_RUNTIME_MODE=production`（本番モード）、`USAGI_RUNTIME_MODE=development`（開発モード）、または `USAGI_RUNTIME_MODE=local`（ローカルモード）で明示する。production は `$USAGI_HOME` または `~/.usagi` 自体を使い、development はその `dev/` 子 directory、local はその `local/` 子 directory を使う。環境変数を未指定または不正な値にした場合は、debug / release build とも local を既定にする。本番モードは `USAGI_RUNTIME_MODE=production` による明示指定が必要である。プロジェクト内の runtime state も同じ定義を使い、production は `<project_root>/.usagi/`、development は `<project_root>/.usagi/dev/`、local は `<project_root>/.usagi/local/` に保存する。旧 `device/` / `develop/` / `development/` との互換処理は行わない。
したがって development 中・local mode 中に本番用の record / locator / lock / daemon-owned state へ触れず、必要なら `USAGI_RUNTIME_MODE=development cargo run` で local mode のまま開発用状態を選べる。`USAGI_HOME` を明示しても同じ分離を適用する。daemon が起動する Agent の MCP server には選択した mode も転送するため、Agent の完了報告も同じ daemon に届く。

開発環境に [Task](https://taskfile.dev/) を導入している場合、リポジトリルートの `Taskfile.yml` から mode を選んで起動できる。`task run` は local mode、`task dev` は development mode、`task prd` は release build の production mode を使う。daemon の再起動は `task daemon:restart`、`task daemon:restart:dev`、`task daemon:restart:prd` を使う。各 task は `USAGI_RUNTIME_MODE` を明示するため、呼び出し元の環境変数には影響されない。

managed session state は repository 内の `.usagi/` ではなく、この shared daemon directory に保存する。最初の
起動時だけ従来の `<repository>/.usagi/lifecycle-state.json` があれば `sessions.json` へ atomically 移行して削除する。lifecycle
state が無い場合は、検証済みの project runtime state（debug は `<repository>/.usagi/dev/state.json`）の session も available record として同じ atomic write で採用する。
この adoption は worktree effect を実行せず、既存 `sessions.json` があれば legacy state を読まず、その durable state を変更しない。
`state.json` に残る display name、origin、notes、PR、last-active は UI-only metadata であり、TUI は同名 managed session へ読み取り結合する。
以後の restart は起動 cwd に関係なく、同じ file に保存された trusted root を session runtime と generic terminal の `login-shell` profile の両方に使う。

既存の `sessions.json` に legacy session を追加する必要がある場合だけ、operator は `usagi session recover-legacy` を実行する。これは dry-run で candidate 名と検証結果だけを表示し、`--apply` を付けた明示操作だけが adoption を永続化する。daemon restart、TUI sidebar refresh、通常の MCP session tool は recovery を呼ばない。MCP の `session_recover_legacy` も同じく `apply: true` がなければ dry-run である。

apply は legacy record 全件の name、期待 path、linked worktree、canonical path、`git worktree list --porcelain` の `usagi/<name>` branch binding を検証する。legacy 内の重複、欠損・不正 record、Git 検証失敗、既存 v2 session との同名（available / creating / deleting / failed を問わない）、または revision 競合は fail-closed となり、`sessions.json` を変更しない。成功時は既存 v2 record と stable ID を保持したまま、検証済み全 record を fresh stable IDs の available session として単一 atomic write で追加する。legacy UI metadata は read-only のままである。

`daemon.json` は `pid`、OS の `process_start_identity`、`started_at` を持つ。この lifecycle record は durable
incarnation fence であり、stale cleanup と conditional clear は record 全体を比較する。identity field を持たない legacy
record は読み取り可能だが owner unknown であり、自動 signal・stale reclaim・replacement start を行わない。
`current.json` の型は generation、daemon directory からの
相対 endpoint、`active` または `draining` の state を持つが、shipping bind は常に `active` を即時 publish し、
standby / draining registry としては使わない。current locator と socket endpoint は永続データではなく、
planned daemon generation の終了時に owner が両方を回収する。locator の atomic publication、crash/failure 後の
復旧、generation-fenced retire の契約は [4. IPC の Unix transport](04-ipc.md#unix-transport) を正本とする。
`bootstrap.lock` / `daemon.lock` / `record.lock` / `current.lock` は空の安定した同期 node として残る。この 4 node の
secure create / reopen 契約は共通であり、本節を正本とする。各 path は `O_NOFOLLOW | O_CLOEXEC` で開き、作成 fd を
`create_new` と syscall mode `0600` で作成し、`fchmod(0600)` してから regular file、effective UID、`nlink == 1`、
exact `0600` と `FD_CLOEXEC` を fd 上で検証する。reopen も同じ flags と invariant を要求する。restrictive umask `0777` または
create と `fchmod` の間の abnormal exit が残した、permission bit が `0600` の部分集合である node（mode `000` を含む）は、
private owner directory 内の exact path が symlink でない owner-owned regular single-link file である場合だけ `0600` へ
修復して secure reopen する。group / other bit を持つ broad mode、symlink、hardlink、non-regular node、owner / inode
replacement は修復せず拒否する。唯一の migration 例外として、origin/main の旧実装が残した exact `0644` の
`bootstrap.lock` は owner-owned regular single-link file である場合だけ一度 `0600` へ正規化する。`daemon.lock`、
`record.lock`、`current.lock` は `0644` を含む broad mode を修復しない。

fd の lock 取得後には pathname を再度 `lstat` し、path と locked fd の device / inode が一致し、双方が上記 invariant を
満たすことを検証する。open と `flock` の間に path が swap / recreate されて別 inode になった場合は fail closed とし、
別々の inode を二人の writer が同じ lock として使うことを許さない。private directory 自体の mode-limited create と
trusted repair の契約は [4. IPC の Unix transport](04-ipc.md#unix-transport) を正本とする。

`terminals.json` と `agents.json` は source-of-truth snapshot として、writer ごとの一意 temporary file に
書き込み・fsync した後に rename で置換する。rename 後は対応可能な platform で parent directory も fsync
するため、途中の snapshot を公開せず、電源断後にも rename を永続化する。保存に失敗した場合は既存の
snapshot を置換せず、失敗した temporary file を削除する。

この full-snapshot write は、`daemon.lock` により daemon process が一つだけである現在の single-writer 契約を
前提にする。一意 temporary と atomic rename は partial JSON を防ぐが、複数 process が同じ古い snapshot を
load して別々に置換した場合の lost update は防がない。現行 store には cross-process の read-modify-write lock、
revision CAS、reload/merge はないため、active / draining を同時起動する前に owner generation ごとの write authority
を分離する必要がある。この未実装の前提は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) で追跡する。

daemon restart 時は `agents.json` と `terminals.json` を spawn admission より前に読む。Agent runtime は
coordinator、semantic operation ledger、safe outcome を hydrate する。両 snapshot の未終端 runtime は
`identity_unknown` の reconcile state に atomic に移す。通常の Agent operation は `ownership_unknown` outcome
とし、committed resume final は source / replacement relation と完全な `TerminalRef` を保持して replay する。
死んだ daemon の PTY master は復元不能であり、PID だけでは child の所有権を証明できないため、この遷移は
attach、input、resize、kill、replacement spawn を行わない。runtime は元の workspace / session / worktree /
daemon generation / operation fence を保持したまま inventory に `live: false` として投影する。`exited` runtime
はそのまま残り、Agent の success / non-zero-exit outcome は同じ意味で replay する。

production の Agent / generic child に保存する `ProcessIdentity.start_identity` は現在固定文字列であり、PID reuse と
別 process incarnation を区別する OS identity ではない。したがって planned rollover の owner 証明、cross-generation
kill、capacity release には使わない。実 process-start / process-group identity への置換は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) で追跡する。

旧 snapshot は次の launch による保存でも削除しない。snapshot の JSON 破損、未知 schema、重複 operation、
scope / generation / operation fence の不整合、または reconcile write failure は daemon startup を fail closed
にする。この状態では runtime を公開せず、spawn も snapshot の上書きも行わない。reconcile 件数と store failure
は日次 error log に記録され、session lifecycle vocabulary は変更しない。旧 Agent schema は欠けた semantic key
や outcome から成功を捏造せず、該当 operation を `identity_unknown` の非 spawnable safe failure として現 schema
に移行する。credential は hydrate せず、restart 後も ephemeral に失効する。旧 PTY 自体は resume せず、利用者には
inventory の `live: false` と typed safe error で非 live を明示する。

## PR refresh scheduler

PR refresh/freshness 契約の正本はこの節である。daemon は committed terminal output から検出した canonical
GitHub PR URL を `pr-inventory.json` に保存し、単一の低優先度 worker が 250 ms ごとに schedule を進める。
同じ URL が複数 chunk または複数 session から登録されても scheduler は identity 単位で coalesce し、1 tick
につき最大 2 identity を canonical URL 順に claim する。remote provider は shell を介さない固定 argv の
`gh pr view <canonical-url> --json title,state` で、1 request を 5 秒で打ち切る。provider 実行中は inventory lock
を保持しないため、slow provider が terminal output の commit や IPC snapshot を停止させない。

| 結果 | durable snapshot | 次回 schedule |
|---|---|---|
| discovery | last-known state を `open`、refresh state を `pending` として追加する | 即時 |
| success | safe title/state を全対象 session に publish し、refresh state を `idle` に戻す | 60 秒後（freshness window） |
| provider / parse failure | last-known title/state を保持し、refresh state を `backing_off` にする | 2 秒から倍増し、最大 60 秒 |

schedule と retry attempt は process-local であり、inventory が durable SSoT である。daemon restart は pin 済み・
dismissed を除く保存済み identity を canonical URL 順に即時 schedule へ rebuild する。これにより wall clock や
前 process の一時 timer に依存せず、同じ snapshot から同じ順序で再開する。shutdown signal を受けた worker は
新しい claim を止める。実行中 request も 5 秒の provider timeout で bounded であり、process 終了後に publish
されない。

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
registry は terminal grid / scrollback の唯一の権威でもある。terminal ごとに VT screen を 1 つ持ち、
受理した全 byte を feed し、resize で screen を reshape するため、attach / resync / resize の snapshot は
bounded journal の先頭位置に依存しない完全な screen state を返せる（wire 表現と revision の正本は
[4. IPC](04-ipc.md#snapshot-payload-と-revision)）。screen の retention は per-terminal の cell budget と
process-local の aggregate cell budget で bound し、超過分は古い scrollback から trim して trim 行数を
counter に計上する。checkpoint payload が frame budget を超える場合も payload 側の古い scrollback を
落として収め、可視 grid だけでも収まらないときは部分的な screen を返さず fail closed とする。

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

この tombstone は exact `TerminalRef`（daemon generation・terminal・workspace・optional session・worktree を
全て含む）を key に retain され、その `TerminalRef` が retention identity を兼ねる。`completed_inventory` は
generic / Agent 両 owner の `Exited` record だけを列挙し、[4. IPC](04-ipc.md#exited-tombstone-visibility) の
`CompletedTerminalEntry` として返す。running / reserved / reconcile 中 / reclaimed は tombstone ではないため
列挙しない。daemon restart 後は未終端 record が `identity_unknown`（`live: false`）へ reconcile され `Exited`
ではなくなるため、completed inventory には現れない。tombstone の lifetime 上限（aggregate retention と GC）は
[#526](../.usagi/issues/526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) が所有する。

tombstone の可視状態は daemon が唯一の authority として保持する **workspace-global visibility** である。root IPC
server は全 client connection で共有する 1 つの visibility ledger を持ち、connection ごとに生成される terminal owner
がそれを参照する。ledger は exact `TerminalRef` を key に `unobserved < observed < dismissed` の monotonic lattice と
`revision` を保持し、`observe` / `dismiss` の compare-and-swap で更新する。したがって複数 TUI process や再 open は
同じ結果へ収束し、late / out-of-order な write が completed entry を復活させない。visibility は表示 intent であり、
runtime の liveness・PTY ownership・process には一切作用しない（CAS / merge の正本は
[4. IPC](04-ipc.md#exited-tombstone-visibility)）。

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

Agent runtime は daemon 所有の Agent owner が持つ。owner は production composition が生成した generation coordinator、durable runtime coordinator、Codex / Claude
adapter を解決する code-defined adapter registry、durable runtime store、実 PTY adapter、producer-issued
`OperationId` の idempotency ledger を一つに束ねる。[`agent` launch request](04-ipc.md#agent-launch-request)
は [managed session scope](#authority-と-lifecycle) を解決してから registry で profile を選び、adapter が
one-shot で provision した public launch plan だけを durable snapshot に保存する。argv、environment value、
secret、raw provision error は wire event・snapshot・TUI feedback に現れない。

generation coordinator は Agent admission、terminal control/exit、completion outcome の単一 authority である。
Agent owner は coordinator から active generation を取得して `TerminalRef` と `CompletionFence` を作り、別の
generation field や terminal binding map で owner を再構成しない。各 transition は runtime record と ownership を
同じ `agents.json` snapshot に保存するため、一方だけが新 generation を指す状態は publish されない。

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

### Provider-native conversation resume

usagi の `SessionId` / `TerminalRef` と provider-native conversation identity は別の型である。daemon は
conversation lineage ごとに secret-free な `AgentContinuationRef` を一度だけ発行し、runtime incarnation ごとに
別の opaque `AgentResumeSourceId` を発行する。前者は live / interrupted / replacement runtime に共通し、後者は
resume source を exact に fence する。いずれも daemon restart を越えて durable だが、provider-native ID ではなく、
新しい lineage や runtime incarnation へ再利用しない。replacement record は `resumed_from`、source record は
`superseded_by` を同じ atomic snapshot に保存する。片側だけの relation、重複 source ID、lineage 不一致は startup
validation error である。

各 Agent runtime record は利用可能な場合だけ `ProviderResumeRef` を持ち、provider、opaque native session ID/name、adapter revision、完全な launch scope、capture provenance、last-known status / safe phase を保存する。native ID の `Debug` は redacted とし、IPC、status projection、response、event、error、日次 log へ出さない。Codex では [private structured capture request](04-ipc.md#codex-structured-capture-request) の入力だけが native ID を一度 IPC で運び、durable ID はこの専用 field だけに保存する。public `LaunchPlan.argv`、再現用 `LaunchRequest`、environment、transcript 本文、raw CLI output には複製しない。redaction が保証するのはこれら durable snapshot・IPC・projection・log の各面であり、provider ID は spawn 時の一時 provision として子 process の argv に載るため、同一 host の process 一覧には露出し得る（provider CLI の入力契約上不可避）。

Claude の新規 interactive launch は daemon が UUID を発行して spawn 時だけ `claude --session-id <uuid>` を追加し、再開時は検証済みの同一 ID を `claude --resume <id>` として一時 provision に追加する。Codex の新規 interactive launch は、adapter-private config に `SessionStart` の `startup` command hook と hidden `usagi codex-session-capture` command を注入する。Codex が documented hook JSON の stdin に渡す current `session_id` だけを、同じ process が継承した daemon-minted credential で exact live runtime に束縛し、structured capture 境界へ渡す。境界は `ProviderCaptureProvenance::ProviderStructured` で永続化し、再開時は検証済みの同一 ID を `codex resume <id>` の一時 provision に追加する。

この Codex 経路の互換条件は、lifecycle hooks、`SessionStart` command event、その共通 input field `session_id`、および daemon が指定する hook trust bypass を CLI が提供することである。managed policy による hooks 無効化、非対応 CLI、hook の skip / timeout / non-zero exit、JSON・event name・ID・credential の欠落/不正、daemon/persistence failure のいずれでも `ProviderResumeRef` を作らず、resume 不可のまま fail-closed にする。hook input の `transcript_path` は deserialize 対象にせず、provider state / transcript / state database / 設定 / 履歴 file の場所や形式を推測・走査・parse する capture 経路も持たない。native ID/name は先頭 `-` の option-like 値を拒否し、`--last` / `--continue` の暗黙選択へ CLI parse が切り替わる余地を持たない。

workspace 単位の `AgentInventory` は root と managed session、同一 scope の複数 history を別 item として
deterministic に返す。resumable projection は availability と非機密な reason を返し、provider-native ID は返さない。
`AgentResumeTarget` は continuation、source、workspace、optional session、worktree、source runtime incarnation、
adapter revision だけを持つ。旧 schema record は continuation / source を合成せず、target 無しの unavailable item
として起動可能なまま読む。

利用者が [`ResumeAgent`](04-ipc.md#provider-conversation-resume-request) を明示したときだけ、daemon は exact
target の全 public fence を durable source record と完全一致で照合し、provider/profile、native identity、capture
policy、adapter revision、workspace / optional session / worktree、current adapter capability を再検証する。source は
non-live の interrupted / exited / reclaimed runtime でなければならず、同じ continuation の live / reserved /
ownership-unknown replacement、同じ source の in-flight resume、metadata 欠落、stale target は spawn 前に拒否する。
capacity と operation fence の獲得後、選択した source 一件だけを `reclaimed` へ supersede し、他 history は変更しない。
成功時は新しい daemon-owned PTY、`AgentRuntimeId`、完全な `TerminalRef` を生成し、同じ continuation と明示的な
source → replacement relation を返すため、crash 前 PTY への再 attach ではない。

producer `OperationId` と target 全体を semantic key にして dedupe する。duplicate click、reconnect、daemon restart
後の replay は同じ durable final / relation / `TerminalRef` へ収束し、新しい spawn や capacity reservation を作らない。
別 target への operation 再利用は idempotency conflict とする。同じ exact target を別 operation で再送した場合は
`superseded_by` の replacement outcome を replay し、failed / in-flight / live / completed のいずれも最初の final から
分岐させない。legacy session-scoped request は現 wire generation の互換期間だけ、eligible exact target が厳密に 1 件の
場合に限って変換する。0 件または複数件を safe typed failure にし、「最新」や provider 種別で選ばない。

daemon restart reconciliation は unfinished record の provider status を `interrupted` にするが、自動 resume は行わない。TUI 起動、pane inventory 復元、daemon / macOS 再起動も同様である。schema v1/v2/v3 record は provider metadata または public lineage が欠けたまま schema v4 として読めるが、ID を推測して補完せず resume 不可のままにする。fixture は continuation の restart stability / non-reuse、root と複数 session、同一 scope の複数 history、Claude UUID、structured Codex capture、scope/revision/incarnation mismatch、ID の public plan argv / snapshot / IPC 非露出、source relation、operation restart replay と exact source の一度だけの spawn を確認する。

restart 後の Agent owner は hydrate 済み operation を admission より先に照合する。同じ semantic intent は保存済み
accepted / completed / safe failure を replay し、同じ `OperationId` の異なる intent は
`idempotency_conflict` にする。新規 launch の snapshot write は hydrate 済み全 record を含むため、過去の terminal、
binding generation、outcome を脱落させない。

```text
startup: agents.json -> validate runtime/ownership binding
                    -> old active owner = retired + identity_unknown
                    -> hydrate GenerationCoordinator
                    -> activate and persist current generation
                    -> open admission

runtime: admission/control/exit/outcome
                    -> exact generation + terminal/runtime fence
                    -> atomic agents.json snapshot
```

旧 generation の terminal command、late exit、late completion は owner lookup から別 runtime へ fallback せず
effect 0 となる。process identity の欠落や runtime/ownership binding の破損は owner を推測せず daemon startup または
当該 effect を fail closed にする。schema v1/v2 の legacy snapshot は runtime fence を保持した
`identity_unknown` ownership に移行し、自動 replacement を行わない。

[`terminal inventory`](04-ipc.md#generic-terminal-request) request も shared terminal owner が処理し、
generic owner と Agent owner の両方に scope を問い合わせて結果を merge する。したがって列挙には generic
terminal と Agent terminal の両方が含まれ、各エントリは `TerminalRef`・`kind`・`live`（現 generation が所有し
attach 可能か）だけを持つ。これは client が workspace open 時に live runtime を pane へ復元するための source of
truth である（[3. TUI](03-tui.md#workspace-open-時の-pane-復元) を正本とする）。

Codex / Claude の Agent launch は `McpWiring` capability を要求し、daemon 自身の絶対パスで `usagi mcp` を
子 MCP server として起動する。製品ごとの MCP 設定は adapter provision が spawn 時だけに渡すため、設定 payload は
public launch plan、durable snapshot、IPC response に残らない。注入した usagi MCP tool は agent が確認なしで
呼べる。Codex は spawn 時に `mcp_servers.usagi.default_tools_approval_mode = "approve"` を渡し、注入した
`usagi` server だけを事前許可する。子 server には `USAGI_HOME` と caller credential だけを forward する。
詳細な MCP caller contract は [7. MCP サーバ](07-mcp.md#起動と経路) を正本とする。Claude も注入した `usagi`
server のツールだけを事前許可する（`--allowedTools mcp__usagi`）。したがって他の MCP server・shell・ファイル編集・
network の permission model は通常どおり維持され、無効化・緩和しない。

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

daemon process は composition 時に metrics broker を一つだけ生成する。subscriber の登録・解除、
bounded queue への publish、drop 集計、wire snapshot はこの broker が一元管理し、IPC connection close
でも同じ登録を回収する。daemon restart は新しい process-local broker incarnation を生成するため、
subscriber と drop count は 0 から始まり、client は再接続後に新しく subscribe する。

同じ snapshot は terminal retention で trim / coalesce した byte 数と、PTY observation queue で
backpressure した byte 数も process-local counter として返す。counter と log は byte 数だけを扱い、
terminal output、argv、environment、secret を含めない。

TUI は最新 snapshot を workspace の左ペイン下部にある v1 互換の usagi mascot の足元の右へ表示する。
この観測値は操作対象ではないため、狭い terminal では session 一覧と footer を優先して mascot ごと省略される。

## generation と orphan safety

generation coordinator は一つの daemon process 内で Agent admission、terminal control/exit、completion outcome を
current generation に fence する。shipping `serve` は process lifetime の `daemon.lock` を保持するため、同じ data
directory で 2 process は共存せず、production lifecycle は coordinator の `rollover` を呼ばない。standby endpoint、
cross-process generation registry、draining process への admission は現在存在しない。generic terminal runtime も
この coordinator の cross-process authority には含まれない。

daemon owner process の exact identity と fenced SIGTERM は lifecycle record に実装されている。そのため PID reuse や
legacy record を daemon owner と推測しない。

generation record は optional `expected_build` を持つ。process-local / legacy record は unknown のまま復元できるが、
cross-process standby contract の `register_standby_for_build` は known identity を必須とする。readiness 後の
`verify_standby_build` は expected artifact と実 `ServerHello` artifact の exact match だけを受理する。unknown または
mismatch は active generation を変えず、candidate を standby のまま保つ。known expected build の record は
`build_verified` が立つまで `rollover` も拒否する。この pure contract を #516 の registry CAS / private endpoint
consumer が使い、TOCTOU で別 artifact を active にしない。

そのため current generation の exact fence は stale request の誤適用を防ぐが、planned restart 中に旧 PTY を
draining owner として維持する機構ではない。安全な landing order は、artifact identity / trigger の
[#528](../.usagi/issues/528-fix-daemon-build-artifact-identity-safe-rollover-trigger.md) と daemon identity / locator の
#515 を前提に、cross-process registry / admission の
[#516](../.usagi/issues/516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)、owner shard /
exit-capacity handoff の
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md)、draining owner routing の
[#508](../.usagi/issues/508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md)、shipping lifecycle / final E2E の
[#507](../.usagi/issues/507-fix-daemon-planned-restart-active-draining-generation-rollover.md) の順である。#508 capability と
compatible registry revision が無い限り #507 の rollover path は disabled とし、old active/current を維持する。

spawn reservation は process spawn より先に保存する。crash 後に process identity を証明できない terminal は
`identity_unknown` として扱い、replacement spawn、input、kill を自動で行わない。PID の生存だけでは ownership
を証明しない。daemon crash をまたぐ PTY master FD の継続はこの契約に含めず、
[PTY broker／FD handoff の調査](proposals/07-pty-crash-continuation.md) に分離する。
