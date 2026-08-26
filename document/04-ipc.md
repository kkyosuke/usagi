# 4. daemon IPC

> [ドキュメント目次](README.md) ｜ ← 前へ [3. TUI](03-tui.md) ｜ 次へ → [5. daemon](05-daemon.md)

daemon と各 client 面が共有する IPC の現在の契約である。クレート境界と実装の置き場所は
[2. アーキテクチャ](02-architecture.md) を正本とする。

## 目次

- [identity と fence](#identity-と-fence)
- [frame と handshake](#frame-と-handshake)
- [workspace fence](#workspace-fence)
- [attempt deadline と reconnect budget](#attempt-deadline-と-reconnect-budget)
  - [terminal lane の per-request budget](#terminal-lane-の-per-request-budget)
  - [bootstrap section の bounded wait](#bootstrap-section-の-bounded-wait)
- [envelope とエラー](#envelope-とエラー)
- [owner generation routing](#owner-generation-routing)
- [daemon rollover request](#daemon-rollover-request)
- [Unix transport](#unix-transport)
- [client の失敗処理](#client-の失敗処理)
  - [stream connection の共有と subscription の無効化](#stream-connection-の共有と-subscription-の無効化)
- [managed session request](#managed-session-request)
- [daemon metrics](#daemon-metrics)
  - [agent concurrency projection](#agent-concurrency-projection)
- [PR inventory snapshot](#pr-inventory-snapshot)
- [agent launch request](#agent-launch-request)
  - [agent operation identity と final の相関](#agent-operation-identity-と-final-の相関)
- [Codex structured capture request](#codex-structured-capture-request)
- [agent phase report request](#agent-phase-report-request)
- [dispatch request](#dispatch-request)
- [generic terminal request](#generic-terminal-request)
  - [snapshot payload と revision](#snapshot-payload-と-revision)
  - [terminal input identity と cross-connection replay](#terminal-input-identity-と-cross-connection-replay)
- [exited tombstone visibility](#exited-tombstone-visibility)

## identity と fence

v2 の resource identity は lowercase canonical UUID の newtype である。表示名、path、PID、
daemon 内 counter は属性であり、effect を行う resource key ではない。`WorkspaceId`、`SessionId`、
`WorktreeId`、`TerminalId`、`AgentRuntimeId`、`AgentResumeSourceId`、`DaemonGeneration` は resource
incarnation ごとに新規発行される。`OperationId` は UUIDv7 の durable intent identity である。
`AgentContinuationRef` は provider conversation lineage ごとの daemon-issued public identity であり、
live runtime、中断した resume source、resume 後の replacement runtime に共通する。provider-native ID
とは別の opaque UUID であり、新しい conversation lineage へ再利用しない。

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

active / standby daemon は accept 後から `ServerHello` または handshake error の write 完了までを
**pre-handshake** として generation ごとに最大 32 connection だけ admit する。上限を超えた socket は worker thread と request state を作る前に
close する。hello をまだ読んでいない相手へ新しい error frame は送らないため wire state は増えず、daemon の error log には
peer data・credential・workspace を含まない `capacity exhausted` を記録する。

handshake 成否にかかわらず、accept 済み worker の総数は generation ごとに process の `RLIMIT_NOFILE` から算出した上限に収める。
128 descriptor を PTY・store・wake pipe・listener・child 用に予約し、worker 1 件の reader / writer / retirement descriptor を 3 件として
残りから上限を求め、thread 数を守るため最大 256 とする（limit を取得できない場合は 32）。finished worker を先に reap し、
上限中は新しい socket を thread 作成前に close する。したがって正しい hello を送った後に idle し続ける同一 UID client も
thread / socket descriptor を無制限には保持できない。この総数上限は established connection の時間制限ではなく、接続終了で枠を返す。
capacity refusal の error log は飽和区間ごとに 1 回だけ記録し、reconnecting client 自身が log / disk pressure を増幅しない。

admit した pre-handshake connection は、prefix read、body read、hello validation、reply write を合わせて 2 秒の単一の
monotonic completion deadline を持つ。各 socket read / write はその絶対時刻までの残量だけで待つため、partial prefix や
partial body の到着で deadline は延長されない。timeout、truncated frame、invalid hello、reply write failure は socket を
close して permit、thread、複製 FD を回収し、理由を秘密を含まない `deadline exceeded` または
`invalid or incomplete hello` として記録する。handshake が成功した時点で permit と socket deadline を外すため、admit 済みの
subscription / terminal lane の寿命や idle policy にはならない。shutdown / generation retirement は
[5. daemon の client worker barrier](05-daemon.md#client-worker-の保持)で pre-handshake を含む全 worker を unblock / join する。

`ClientPolicy.timeout_ms` / `reconnect_attempts` は surface 別（TUI 2s/3、CLI 10s/1、MCP 30s/1）の policy であり、
CLI・MCP・TUI の per-request 経路は [attempt deadline と reconnect budget](#attempt-deadline-と-reconnect-budget) で
これを実効化する。TUI の terminal lane はこの policy より小さい
[per-request budget](#terminal-lane-の-per-request-budget) を持つ。TUI の pane restore は request を off-thread に
隔離して frame / input / quit の同期待ちを避ける。

最初の frame は必ず `ClientHello` である。hello は client ID、connection nonce、期待する
daemon generation、client が申告する workspace（[workspace fence](#workspace-fence)）、対応 protocol range、
capability、build diagnostics を含む。daemon は「意図した daemon か」を先に確定するため
**generation → workspace → protocol / capability** の順に検証し、成功時に `ServerHello` を返す。build identity は wire
protocol の互換性判定には使わないが、client bootstrap は `ServerHello` の identity で同一 runtime channel の
daemon が現在 executable と **exact same artifact** かを確認する。client は `build.artifact.v1` capability を必須とし、
capability を持たない旧 daemon は build tuple へ fallback せず handshake で拒否される。

`BuildIdentity` は version、commit diagnostics、full target triple、canonical `artifact` を持つ。artifact は
`usagi-artifact-v1:<profile>:<target>:<source-id>` である。`build.rs` は Git checkout では
commit と tracked / untracked source set、Git metadata の無い package build では package source set から
path 非依存の source identity を作り、Rust compiler version、feature、rustflags、profile、target も digest に含める。
identity は binary の compile-time constant であり、daemon は process startup から同じ値を広告して executable path を
読み直さない。このため self-update が同じ path を
atomic replace しても、old process は startup artifact を広告し続け、新 process だけが replacement artifact を広告する。
wire / log に absolute build path、user name、secret は載せない。

release / distributed / development / local はすべて exact artifact 一致時に existing daemon を再利用する。同じ
version / target でも source tree または build configuration が異なれば `BuildArtifactDecision::RolloverTrigger` となる。
trigger は artifact pair、runtime channel、force bit から決まる stable `OperationId` を持つため、concurrent client、
response loss、reconnect、repeated bootstrap は同じ key へ収束する。trigger の生成は effect-free であり、old daemon、
endpoint、PTY に stop signal を送らない。production / local の bootstrap は trigger を typed outcome として返して
old owner を維持する。development の bootstrap は既知の mismatch trigger に限って **planned replacement** で消費し
（live runtime があれば seamless rollover、無ければ cold transition）、replacement の exact artifact を再接続時の
handshake で確認する。replacement が拒否された場合と、replacement 後も別 artifact が広告されている場合は、
到達可能な old owner を effect 0 で再利用する（[5. daemon の build mismatch](05-daemon.md#authority-と-lifecycle)）。
通常 bootstrap から same-artifact replacement は発行せず、明示
`usagi daemon replace` だけが `ForceReplace` trigger を作る。identity が empty / malformed / unsupported の場合は
version / target 一致へ昇格せず、typed `BuildIdentityUnavailable` として old daemon を維持する。

trigger の生成自体は effect-free だが、`usagi daemon replace` はその trigger を自ら消費し、
`usagi daemon restart` と同じ経路で replacement を実行する。live runtime があれば standby readiness 後に
old active へ `rollover` request を送り、live runtime が無い場合だけ cold transition を行う
（[5. daemon の planned replacement](05-daemon.md#planned-replacement)）。

private standby の起動・登録・readiness は shipping の `daemon serve --standby` が駆動する
（[5. daemon の standby process の lifecycle](05-daemon.md#standby-process-の-lifecycle)）。CLI は verified
standby を確認してから old active へ request を送り、commit 前の failure では staged standby を停止する。
通常 envelope は handshake の成功後だけ受理する。

## daemon rollover request

`DaemonRequest` の lifecycle verb は
`{"kind":"rollover","operation_id":"<durable operation>"}` である。CLI は authority を直接書き換えず、
current old active へこの request を送る。old active は registry の active が自 generation であることを確認し、
登録済み successor の private endpoint へ read-only hello を行い、artifact / handoff / owner-routing capability を
再検証する。その後、connection ledger と planned registry revision を `RolloverPlan` に束ね、自 process の
`AdmissionGate` で gated handoff を実行する。

`rollover` request は active role だけが受理するが `ActiveControl` lease は取らない。trigger 自身がその lease class を
close して drain を待つためである。routing 非対応 client、successor capability 不足、revision mismatch、
readiness failure は最初の handoff write より前に typed error / `side_effect: none` で返り、old active と current を
維持する。registry commit 後の partial phase は durable operation と handoff phase を使って roll-forward / fail closed
へ収束する。

## workspace fence

daemon が権威を持つ workspace root は起動時に 1 つ確定し、以後は client が選んだ workspace を adopt して増える
（[5. daemon#tenant registry](05-daemon.md#tenant-registry)）。一方 client の接続先は data directory
（`$USAGI_HOME` と runtime mode）から解決するため **workspace に依存しない**。
したがって handshake が workspace を照合しなければ、workspace B で実行した client が workspace A の daemon へ
接続し、A の session 一覧・scope・PR inventory をそのまま受け取る（`session remove` は A の worktree を撤去する）。
`ClientHello.workspace` はこれを閉じるための申告であり、daemon は申告から **この接続が扱う workspace（trusted
root）を解決**し、その root と申告を突き合わせて admit / refuse を決める。daemon は複数の workspace を tenant として
保持できるため（[5. daemon#tenant registry](05-daemon.md#tenant-registry)）、解決の答えは起動時に固定された 1 つの
root ではなく、その daemon が保持する workspace のいずれかである。fence 自体は解決の後段にそのまま残り、誤った
endpoint に届いた client を拒否する backstop として働く。

| 申告 | wire | 意味 |
|---|---|---|
| bound | `{"scope":"bound","root":"<絶対 canonical path>"}` | この client は `root` を含む workspace で作業する（process の文脈） |
| selected | `{"scope":"selected","root":"<絶対 canonical path>"}` | この client は `root` の workspace そのものを操作する（TUI が開いた workspace） |
| unbound | `{"scope":"unbound"}` | workspace resource を一切扱わない接続 |
| 欠落 | field 省略 | fence 以前の client。typed error で拒否する |

解決は申告の種類ごとに次のとおりで、**generation fence の後**に走る。順序が逆だと、別 generation を目指した
client が拒否される過程でこの daemon に workspace を adopt させられてしまう。

| 申告 | 解決 |
|---|---|
| `selected` | canonical 化した root を **adopt する**（すでに保持していればそれを使う）。adopt できない workspace はこの接続だけを拒否する |
| `bound` | 保持している workspace のうち、その path を含む**最長一致**を選ぶ。保持していない場合は、この data directory が**かつて開いた** workspace（state subtree の `root.json`）の最長一致を探して adopt する。それも無く、かつ **その path 自身が git repository** ならそれを adopt する。どれにも当たらない path は拒否する（**上位ディレクトリは探索しない**） |
| `unbound` | workspace resource を扱わないので、起動時の workspace を答える |
| 欠落 | 起動時の workspace を答え、下の fence が拒否する |

`bound` の miss は 2 段で解決する。前段は「**存在する** workspace を探す」で、後段は「workspace を**作る**」なので、
必ずこの順に試す。

1. **かつて開いた workspace**（state subtree の `root.json`）の最長一致。ここに当たれば、その workspace は
   一度開かれている以上「利用者が指した」ものである。tenant から idle で退いた workspace
   （[5. daemon#tenant registry](05-daemon.md#tenant-registry)）が、そこで動いている CLI / MCP client を
   拒否し始めるのを防ぐのがこの段である。
2. **申告された path 自身が git repository** である場合だけ、新しく開く。新しく clone した repository を、
   先に TUI で開かずに CLI / MCP から使い始められるのはこの段である。

後段が**上位ディレクトリを探索しない**のは意図である。`bound` は「開く対象」ではなく「動いている場所」の申告
なので、上へ辿ると *たまたま上にあった* repository を開いてしまう。`$HOME` に dotfiles repository を置く構成は
珍しくなく、上位探索を許すと `usagi session create` を home 配下のただの directory で実行しただけで `$HOME` に
fence を取り、`~/.usagi/sessions/<name>` を dotfiles の worktree として作り、dotfiles に branch を切ることになる。
repository に**立っている**ことは、どの workspace を指しているかの明確な表明である。その下のどこかに居ることは、
そうではない。

いったん adopt されれば、その配下はすべて最長一致で同じ workspace に解決される（この 2 段が触るのは「保持して
いない workspace をどう解決するか」だけである）。session worktree（`<root>/.usagi/sessions/<name>`）は自身の
`.git` を持つが workspace ではないので、後段の対象としては常に除外する。それが存在する時点でその workspace は
前段で解決できる。

この制限は **`bound` による暗黙の adopt にだけ**掛かる。「repository でなければ workspace になれない」という規則では
ない。`usagi open <path>` と TUI の Open / New は `selected` を申告する明示的な操作であり、対象が repository か
どうかを問わない。制限の根拠は「repository かどうか」ではなく「利用者がその workspace を指したと言えるか」である。

client が daemon を auto-start する前にも、同じ `bound` 解決を read-only preflight として行う。かつて adopt した
workspace の最長一致、または申告 path 自身が repository の場合だけ、その解決済み root を lifecycle child と
bootstrap broker の cwd にする。どちらでもない場合は handshake と同じ `workspace-mismatch` / effect none を返し、
child、workspace fence、project-local `.usagi` を作らない。`selected` と、選択済み root に対する `unbound` readiness は
明示操作なので従来どおり任意の canonical directory を起動 root にできる。Doctor のように選択を持たない `unbound`
lifecycle probe は ambient cwd に同じ implicit rule を適用する。これにより、同じ `bound` command の可否は daemon の
生死に依存しない。

`selected` の adopt が失敗する理由は 3 つある。いずれも **その workspace だけ**の拒否であり、同じ daemon が保持する
他の workspace の接続には影響しない。

| 理由 | 例 |
|---|---|
| 別の daemon がその workspace を fence している | 別 mode・別 build の daemon が稼働している |
| root が解決できない | 削除された path、非 UTF-8 |
| tenant 上限に達した | 開いたままの workspace が多すぎる |

解決した root と申告の突き合わせ（fence 本体）は次のとおりで、比較は path component 単位である（`<root>-2` は
`<root>` の子ではない。末尾スラッシュや `.` / `..` の綴り差は同じ root になる）。

| 判定 | 条件 |
|---|---|
| admit | `unbound`、`bound` の root が trusted root と一致するか **その配下**（subdirectory・session worktree `<root>/.usagi/sessions/<name>` を含む）、`selected` の root が trusted root と **完全一致** |
| refuse | 別 workspace の root、trusted root の親、`selected` が trusted root の配下（subdirectory・session worktree）、比較できない root（相対 path、非 UTF-8 が畳まれた空 root）、daemon 側の trusted root が空、申告の欠落 |

`bound` と `selected` の違いは「どこで動いているか」と「何を操作するか」である。subdirectory や session worktree は
**動く場所**としては同じ workspace なので `bound` では admit するが、**開く対象**としては別 workspace であり、そこを
`selected` として admit すると「その directory の workspace 名の下に daemon の workspace の session 一覧」を出して
しまう。したがって `selected` は完全一致だけを admit する（[3. TUI#workspace-の選択と-daemon](03-tui.md#workspace-の選択と-daemon) が
選択側の正本）。

拒否は `permission_denied` / `error_id = workspace-mismatch` / `retry_mode = never` / `side_effect = none` の
typed `ProtocolError` であり、message は **その daemon が serve している workspace root を過不足なく列挙する**
（1 つも無ければその旨）。1 つに固定して名乗ると、複数 workspace を保持する daemon が実態と違うことを言い、
adopt に失敗した root をそのまま「serve している」と名乗る自己矛盾した文になる。client はこれを
そのまま提示し、`unavailable` へ丸めない。bootstrap はこの拒否を「到達不能」と解釈しないため、
daemon の cold start、stale endpoint recovery、rollover、cold restart のいずれも起こさない
（別 workspace を正当に所有している daemon を壊さないため）。readiness 待ちも即座に打ち切る。

client は申告する root を次の順で決める。git を client 起動ごとに実行しない。

| 優先 | 出所 | 申告 | 対象 |
|---|---|---|---|
| 1 | 開いた workspace（TUI が `usagi open <path>` / Recent / Open 一覧で選んだ root を canonical 化した値） | selected | workspace 画面とその全 request |
| 2 | `USAGI_WORKSPACE_ROOT`（daemon が provision した child に注入する trusted root） | bound | MCP child、agent hook |
| 3 | process の canonical working directory | bound | CLI・手動起動の `usagi mcp` |

開いた workspace が最優先なのは、それが「この接続がこれから表示・変更する workspace」だからである。cwd や注入
された root は同じ process の別の事実に過ぎず、TUI は cwd 以外の workspace を開ける。

root を申告する client（`bound` / `selected`）は `workspace.fence.v1` capability を必須にする。fence を持たない
daemon はどの workspace の client も admit してしまうため、capability の無い peer は handshake で拒否する
（`build.artifact.v1` と同じ形の双方向 fence）。

**免除する経路**は「workspace resource を一切名指さない接続」だけであり、`unbound` を申告する。

| 経路 | 理由 |
|---|---|
| client readiness（`usagi open <path>` の起動前確認） | daemon の存在確認だけで workspace state を読まない |
| `usagi daemon replace` | 広告された build identity を読む lifecycle 操作であり、workspace に紐づかない |

この fence は**同一 UID の協調する peer 同士の一貫性 fence**であり、authorization boundary ではない
（accept 時に UID を検証済みで、到達できた peer は任意の root を綴れる）。したがって `unbound` は
per-request の権限判定ではなく、「workspace 作業をしない」という申告である。

data directory ごとに daemon は 1 つだが、その daemon は **複数の workspace を同時に serve する**。daemon が
動いていない状態で workspace を開くと、その workspace を serve する daemon が起動する（起動する lifecycle child の
cwd が開く workspace になる。[5. daemon](05-daemon.md#daemon-process-lifecycle) が startup cwd = workspace root の
正本）。既に別の workspace を serve している daemon に対する選択は、その workspace を adopt して admit する。
adopt できない場合だけ typed refusal になり、TUI は理由と復帰手順を提示して、別 workspace の session 一覧を
別 workspace の title で表示することはない
（[3. TUI#workspace-の選択と-daemon](03-tui.md#workspace-の選択と-daemon)）。

## attempt deadline と reconnect budget

client は surface policy を **1 attempt = 1 つの end-to-end monotonic deadline** として実効化する。initial attempt と各
reconnect attempt は、connect / handshake / frame write / response read をまとめて `timeout_ms` の budget から消費する。
deadline は monotonic clock に対して固定した目標時刻であり、frame header / body の部分到着、無関係な event、partial
progress では **reset しない**。したがって peer が hello 前、request read 後、partial response 後、無関係な event の連続中に
停止しても、各 attempt は deadline + わずかな scheduler 誤差以内に typed unavailable で戻る。deadline を超えた
socket は partial frame を持ち得るため再利用せず、connection を破棄する。

`reconnect_attempts = N` は initial の後に高々 N 回の追加 attempt を許す。したがって最大 wall-clock は
attempt 数 × surface deadline に有界な reconnect 誤差を加えた値として計測できる。budget を使い切ると client は
typed `unavailable` を返す。これは side-effect state を definitive failure と断定しない（effect unknown）。

retry を許すかは request class だけで決める。これが唯一の eligibility 判定であり、fail-closed である。

| request class | new connection retry | 根拠 |
|---|---|---|
| read-only query | budget 内で可 | 完全な resource / generation fence で再読でき、stale response は捨てる |
| server-backed durable mutation | budget 内で可 | producer `OperationId` + semantic digest を daemon durable store が照合し、同じ operation final へ収束する |
| `RequestId` だけの mutation | 不可 | `RequestId` は connection-local correlation に過ぎず、cross-connection idempotency evidence ではない |
| terminal input | 不可 | ACK loss は再送ではなく read-only な outcome 照会で収束させる（[cross-connection replay](#terminal-input-identity-と-cross-connection-replay)） |
| terminal input outcome 照会 | budget 内で可 | daemon の operation ledger を読むだけなので、response loss は安全に再照会できる |
| producer `OperationId` 付きの generic Terminal Launch | budget 内で可 | 同じ id + 同じ canonical digest が同じ `TerminalRef` を replay し、異なる digest は `idempotency_conflict` になる |
| producer `OperationId` の無い generic Terminal Launch | 不可 | daemon が request ごとに terminal / operation identity を発行するため、再送は別 record になる |

request 送信前（connect / handshake の失敗）は effect が生じないため、どの class でも budget 内で安全に retry する。
request を送信した後の response loss / deadline では、上表で eligible な class（read-only と durable operation）だけが新しい
connection で retry する。ineligible な mutation は effect unknown を返してその場で終了し、未使用 budget があっても blind
retry しない。durable mutation の retry は新しい effect request を作らず、同じ producer `OperationId` + semantic digest で
outcome を query / replay する。well-formed な `ProtocolError` は server が応答した definitive な結果なので retry せず、
healthy な connection は再利用のため保持する。

late response は attempt ごとに connection が異なり、同一 connection 内でも request sequence が進むため、新しい request に
誤相関しない。

### terminal lane の per-request budget

合成ルートが作る daemon socket は**すべて deadline 付き**である。client は型として 1 つ（deadline transport の上の
`IpcClient`）しかなく、生の socket を掴む経路は存在しない。したがって「deadline の無い lane」は構成上作れない。

attach 済み terminal の lane（attach / resync / input / input outcome / detach / inventory）は**持続的な connection**で、
その connection に全 pane の attachment と exactly-once input ledger が乗る。ゆえに再接続を透過的に行う
[`PolicyClient`](#attempt-deadline-と-reconnect-budget) には載せられない（黙って張り替えると、pane がまだ有効だと思っている
subscription を無効化してしまう）。代わりに lane は connection を保持したまま、**request ごとに deadline を張り直す**。
1 attempt = 1 end-to-end deadline という不変条件は同じで、reconnect は行わない。

| budget | 対象 action | 値 | 根拠 |
|---|---|---|---|
| poll | `resume` / `resize` | 50ms | daemon 側 stateless。落としても次 frame が再要求するだけで失うものが無い |
| input | `input` / `input_outcome` / `detach` | 750ms | keystroke の PTY write と ACK、および失った ACK を解決する read-only 照会 |
| snapshot | `attach` / `resync` / `inventory` / `completed_inventory` / `observe` / `dismiss` | 1000ms | screen checkpoint の直列化や scope 走査を伴い、keystroke より正当に遅い |
| connect | lane の connect + handshake | 1000ms | **1 attempt** であり cold start ではない（`daemon start` 後の readiness 探索は独自の bounded retry を持ち、その attempt ごとに新しい budget を得る） |
| launch | `launch` | 2000ms | process を起こす。lane ではなく per-request の `PolicyClient` 経路に載る |

budget 超過は transport failure として扱う。socket に partial frame が残り得るため lane を破棄し、client-local な
connection epoch を進める。その結果、全 pane が
[stream connection の共有と subscription の無効化](#stream-connection-の共有と-subscription-の無効化)の epoch 経路で
再 attach する。

この「破棄する」帰結が、budget を frame 予算まで小さくしない理由である。lane を落とすと全 pane が再 attach し、その
window に打たれた keystroke は遅れて届くのではなく **feedback 付きで拒否される**。busy なだけの daemon で発火する
budget は、負荷の高いマシンで実入力を失わせる。各 budget は「負荷下でも健全な round trip」より上、「freeze と呼ばれる
時間」より下に置く。描画スレッドの露出そのものをさらに縮めるのは frame 予算の問題であり、
[#551](../.usagi/issues/551-fix-tui-home-frame-loop-daemon-rpc.md) が扱う。

input の budget 超過は **effect unknown** であり、blind retry しない。daemon が既に PTY へ書いた可能性があるため、
client は同じ producer `OperationId` で read-only な `input_outcome` を照会して `final` / `unknown` に収束させる
（[terminal input identity と cross-connection replay](#terminal-input-identity-と-cross-connection-replay)）。
request 送信前に lane を確立できなかった場合は effect が確定的に 0 なので、`unavailable` として扱う。

### bootstrap section の bounded wait

connect / cold start を跨いで 1 データディレクトリに daemon が 1 つだけ立つよう、client は `bootstrap.lock` の
cross-process section を取る。この section と、private directory の setup section（`ensure_private_dir` が親
ディレクトリに取る flock）は、**いずれも blocking `flock` ではなく bounded な `try_lock` retry** である。データ
ディレクトリはマシン全体で共有されるため、blocking にすると MCP server / CLI / rollover のいずれかが section に
いる間、UI 経路の接続確立が無期限に待ってしまう。保持したまま wedge したプロセスがいれば永久に待つ。

| section | 待ち上限 | 上限の根拠 | 超過時 |
|---|---|---|---|
| `bootstrap.lock` | readiness ceiling（40 × 50ms = 2s）＋ spawn margin 3s | section は 1 回の `connect_or_start` を跨いで保持され、最悪ケースは cold start（lifecycle child の spawn ＋ readiness 探索） | typed `bootstrap_contended` |
| private directory setup | 2s | 1 ディレクトリの作成 / 修復だけなので、健全な保持者は microsecond 単位で去る | `would_block` の IO error |

`bootstrap_contended` は `unavailable` と別の typed error である。daemon は健全に動いていて、単に別 client が接続を
確立中というだけの状態を「daemon が居ない」と報告しないためで、retry mode は reconnect、side effect は none
（request は 1 件も書かれていない）、code は `busy` である。TUI はこれを「別プロセスが接続確立中；再試行する」として
表示し、既存の reattach backoff がそのまま再試行する。

## envelope とエラー

通常通信は protocol version と daemon generation を必ず持つ envelope である。

| kind | 相関子 | 用途 |
|---|---|---|
| request | `RequestId` | client の一回の RPC |
| response | 同じ `RequestId` | immediate result、accepted operation、または typed error |
| event | `SubscriptionId`、`StreamRef`、sequence | server push |

現行 production handler で `RequestId` は一回の RPC correlation にだけ使い、`ResponseCache` は接続されていない。
再接続すると client の request sequence も変わるため、`RequestId` を durable idempotency key として扱わない。
session / Agent / dispatch / generic Terminal Launch 等の durable mutation は request correlation と独立した
`OperationId` を持ち、target scope と semantic digest が同じ場合だけ既存 operation として再利用する。producer が
`OperationId` を送らない場合だけ、daemon が自分の operation identity を発行し、その request は cross-connection の
idempotency evidence を持たない。

`ProtocolError` は machine-readable な code、safe message、retry mode、side-effect classification、
error ID を返す。resource/ownership を証明できない場合は `ownership_unknown`、resume が成立しない
場合は `resync_required` を使う。OS error、secret、raw launch provision は error detail に含めない。

## daemon metrics

`metrics` request は daemon の観測用 snapshot を取得し、必要な client が stream を登録または
解除するための control vocabulary である。TUI は `snapshot` を周期的に取得し、push queue を
登録しない。`subscribe` を使う client は受信 queue を drain し、正常終了時には `unsubscribe` を
送る。接続が切れた subscription は connection-local であり、再接続で resume せず新しく登録する。

daemon が送る snapshot は次の versioned schema である。これは表示・診断専用で、TUI が
session / terminal の所有権や local fallback を判断する根拠にはしない。TUI がこの sample 列から作る
診断表示は [3. TUI](03-tui.md#daemon-health-indicator) が正本である。

| field | type | meaning |
|---|---|---|
| `schema_version` | `u16` | metrics payload schema version。現在は `4` |
| `sampled_at_ms` | `u64` | daemon が sample を作成した monotonic timestamp |
| `cpu_percent_hundredths` | `u32` | 前回 sample からの daemon process CPU 使用率（百分率の 1/100 単位） |
| `resident_memory_bytes` | `u64` | daemon process の peak resident memory（byte） |
| `active_subscribers` | `u32` | sample 作成時の observer 数 |
| `dropped_updates` | `u64` | slow observer の bounded queue で coalesce した update 数 |
| `terminal_dropped_bytes` | `u64` | retention window から trim した terminal output byte 数 |
| `terminal_coalesced_bytes` | `u64` | retained segment に連結した terminal output byte 数 |
| `terminal_backpressured_bytes` | `u64` | bounded PTY observation queue の空きを待った terminal output byte 数 |
| `pr_projection_dropped_bytes` | `u64` | deferred projection queue が満杯で PR 走査しなかった committed output byte 数 |
| `pr_projection_coalesced_bytes` | `u64` | 既に queue 済みの projection chunk へ連結した committed output byte 数 |
| `pr_projection_gaps` | `u64` | 落ちた byte を跨いで PR 走査を連結しないために記録した discontinuity 数 |
| `agent_concurrency` | `object?` | Agent concurrency の使用中/上限。報告しない daemon では欠落する（下記） |
| `failed_background_workers` | `u8` | この daemon process で panic して停止した長寿命 maintenance worker の種類数 |

各 subscriber は容量 1 の queue を持つ。daemon は tick で block せず、queue が埋まった
observer の中間 sample を落として count する。切断された observer は次の publish で取り除く。
このため遅い TUI や一つの接続の切断が daemon tick または他 TUI の配信を止めない。

### agent concurrency projection

`agent_concurrency` は daemon が Agent launch を admit する権威そのものの level である。
対象は **Agent runtime の concurrency pool だけ**で、generic terminal capacity や supervisor run の
`ExecutionPolicy.max_concurrency` とは別物であり、他 pool と合算しない。何を使用中と数えるかは
[5. daemon の Agent concurrency projection](05-daemon.md#agent-concurrency-projection) が正本である。

| field | type | meaning |
|---|---|---|
| `in_use` | `u32` | concurrency slot を保持している Agent runtime 数 |
| `limit` | `u32` | daemon が同時に admit する slot 数 |

- 2 つの値は**1 つの object として運ぶ**。別 sample の `in_use` と `limit` を組み合わせて読むことが
  構造上できないため、client 側で「使用中 > 上限」のような矛盾した組を作れない。
- **object 全体が optional** である。schema 3 より前の peer は field を送らないので client は
  `null`（不明）として読む。これは `in_use: 0`（idle と報告された）とは別の状態であり、client は
  不明を idle と読み替えない。
- 未知の field を含む object は無視して読む。したがって planned restart 中に新旧 daemon が併存しても、
  どちらの向きでも payload は解釈できる。
- client はこの level を所有権や admission の判断に使わない（表示・診断専用）。次の launch が拒否されるか
  は daemon が admit の瞬間に決める。

## PR inventory snapshot

`pr` request は stable `SessionId` を対象に daemon-owned inventory の source-of-truth snapshot を返す。
handshake では `pr.snapshot.v1` capability を必須にし、dedicated subscription を提供する peer は
`pr.subscription.v1` も advertise する。

| action / event | fields | contract |
|---|---|---|
| `snapshot` | `session_id`, `revision?` | canonical URL、optional title、state、optional `head_oid`、pin/dismiss と refresh state を含む current snapshot を返す。`head_oid` はGitHubが返すPR head commitで、squash merge済みbranchの削除証明にも使う |
| `subscribe` / `unsubscribe` | `session_id` | connection-local hint subscription を登録・解除する。disconnect は登録を回収する |
| `pr.updated` | `session_id`, `revision` | inventory mutation を示す lossy hint。client は snapshot を再取得して収束する |

revision は session ごとに monotonic である。duplicate、欠落、順序逆転した `pr.updated` は client state
の差分適用根拠にしない。client は最後に見た revision より新しい hint を受けた場合、または reconnect 後に
snapshot を読み直す。slow subscriber は bounded queue で coalesce/drop され、PR refresh、terminal drain、
他 client の RPC を停止させない。

## managed session request

`session` kind の `create`、`remove`、`list`、`overview` は daemon が所有する durable lifecycle に届く。create / remove は producer-issued `OperationId` を accepted response に返し、list / overview は同じ revision 付き workspace snapshot を返す。provider conversation の再開は name-based session action ではなく、[exact target の `ResumeAgent`](#provider-conversation-resume-request) request だけが受け付ける。create / remove の accepted response は snapshot とともに safe final hook を返す。hook は `kind`（`session.created` または `session.removed`）、`operation_id`、`revision` を持ち、TUI は create skeleton を同じ operation の `session.created` hook でだけ終了する。remove の hook は受理を意味し、worktree 撤去の完了ではない（下表）。`OperationId` の再送は action と canonical intent が一致するときだけ同じ operation を返し、異なれば `idempotency_conflict` で拒否する。create intent は canonical session target と role、remove intent は canonical session target、request origin（client request / compensating teardown）、effective `force` を含む。

create の durable outcome と wire response / hook の対応は次の表を正本とする。同じ semantic operation の再送は daemon restart の前後を問わず同じ行を replay し、filesystem / Git effect を再実行しない。

| durable outcome | IPC outcome | final hook |
|---|---|---|
| `succeeded` | `accepted`（同じ `operation_id` / final revision / snapshot） | `session.created` |
| `failed`（effect failure または interrupted reconcile） | safe `error` | なし |
| 同じ `OperationId`、異なる action / canonical intent（target / origin / `force`） | `idempotency_conflict` | なし |

旧 daemon が書いた `remove:<name>` semantic key は origin と `force` を証明しない。対象 session が同じ operation と durable `DeletePlan` を保持し、その plan の `force` と branch 削除 mode が現行 request と一致する間だけ互換 replay する。旧 client remove の branch 保持 plan と現行 client remove の safe branch 削除 plan は、どちらも強制 branch 削除を許可しない同じ client intent として replay できる。完了後など plan が残らない旧 journal は同一 intent と推測せず、同じ `OperationId` の再利用を `idempotency_conflict` で fail closed に拒否する。compensating teardown は常に `force: true` / forced branch delete の内部 intent であり、通常の client remove と相関しない。

**remove の応答は durable outcome ではなく受理の時点で返る**（[5. daemon の session teardown
worker](05-daemon.md#session-teardown-worker) が正本）。worktree 撤去は daemon 所有 worker が続けるため、応答が返った
時点では session は snapshot 上に `deleting` として存在する。

| remove request | IPC outcome | final hook |
|---|---|---|
| 受理（`available` / `failed` な session を `deleting` へ遷移） | `accepted`（`operation_id` / 遷移後 revision / `deleting` を含む snapshot） | `session.removed`（＝受理。撤去完了ではない） |
| すでに `deleting` な session への新しい `OperationId` | `accepted`（**進行中 operation の** `operation_id` と現在の revision / snapshot） | `session.removed` |
| 受理済み operation の同一 canonical intent 再送 | teardown 実行中または成功なら `accepted` の replay、失敗なら safe `error`（保存済み summary） | `accepted` 時 |
| 未知の session、不正な `force`、非 canonical `OperationId` | safe `error` | なし |
| 同じ `OperationId`、異なる action / canonical intent（target / origin / `force`） | `idempotency_conflict` | なし |

teardown 自体の結果は、この応答ではなく後続の `list` / `overview` snapshot で観測する（`deleting` 行の消滅＝完了、
`failed` 行＝失敗と理由）。

snapshot の session は `WorkspaceId`、`SessionId`、`WorktreeId`、lifecycle を含み、workspace 全体の **root `WorktreeId`**（`⌂ root` の scope 識別子）も含む。agent / terminal 起動用の checkout path は、daemon が available の完全一致 scope（managed session、または `session_id` を持たない workspace root）からだけ解決する。client が name または path を渡して scope を再探索する wire contract はない。

## agent launch request

`agent` kind は daemon 所有の Agent runtime に届く。client は producer-issued `OperationId` と、`WorkspaceId` / optional `SessionId`（省略時は workspace root）/ optional profile ID だけの launch intent を送る。worktree、checkout path、profile 既定値、argv、environment、secret は wire field ではなく、daemon が [managed session scope](05-daemon.md#authority-と-lifecycle) と code-defined adapter registry から解決する。profile を省略すると daemon の既定 policy が選ぶ。

daemon は intent の `(WorkspaceId, SessionId?)` を [available scope](05-daemon.md#authority-と-lifecycle) の完全一致に解決し、その worktree だけを launch に使う。`SessionId` を省略した intent は workspace root に解決し、cwd を trusted repository root にする。creating / deleting / failed / stale / mismatch の scope、未知 profile、canonical でない `OperationId` は PTY を spawn せず typed safe error になる。

成功した launch は accepted response に producer `OperationId` と durable revision を返し、body に完全な `TerminalRef` と新しい `AgentContinuationRef` を載せる。この `TerminalRef` は operation・workspace・session・worktree・daemon generation・terminal incarnation を fence する。PTY exit を daemon が一度だけ記録すると、同じ semantic intent の再送は成功時に `completed: true` と同じ `TerminalRef` を持つ final response を返す。non-zero exit は安全な `unavailable` final として replay される。同じ `OperationId` を異なる intent で送ると `idempotency_conflict` になる。spawn failure・ambiguous・persist-after-spawn は fenced safe failure（`unavailable` / `ownership_unknown`）として durable に記録され、resend は同じ安全な失敗を replay する。replacement spawn や terminal の推測は行わない。

この replay 契約は daemon restart をまたぐ。fresh daemon は Agent snapshot の load、generation coordinator と operation ledger の hydrate、新しい process-local generation の atomic activate が完了するまで request admission を開始しない。旧 daemon process はその前に終了しており、PTY master は移送されない。Agent runtime record と generation/terminal ownership は自 generation の owner shard（`shards/<generation>.json`）が同じ compare-and-swap で持ち、admission、terminal command、exit、completion はすべてこの process-local authority を通る。restart 時に所有権を証明できない未終端 runtime は `identity_unknown` として inventory に `live: false` で現れ、旧 `TerminalRef` の command と late outcome は effect なしで拒否される。runtime と ownership binding の不一致、破損、未知 schema は daemon startup を fail closed にし、Agent spawn と snapshot 更新を行わない。schema v1/v2 は既存 runtime fence を保持した `identity_unknown` へ保守的に移行する。MCP caller credential は replay 対象ではなく restart で失効する。

```text
Agent request / PTY observation / completion
                  |
                  v
       GenerationCoordinator (process-local authority)
        | active generation admission
        | exact TerminalRef control/exit
        | exact CompletionFence outcome
                  |
                  v
 shards/<generation>.json = generation ownership + runtime records (atomic)
```

Agent の pending pane は、同じ `OperationId` の成功 final が返した `TerminalRef` にだけ attach する。attach 以降の stream（`attach` / `resume` / `resync` / `input` / `resize` / `detach`）は [generic terminal request](#generic-terminal-request) と同じ vocabulary を共有し、daemon は `TerminalRef` の所有元（agent または generic）へ透過的に routing する。この pending pane の attach policy は [3. TUI](03-tui.md) を正本とする。

### agent operation identity と final の相関

`agent` の答えを producer 側の 1 operation へ相関させる規則は本節が正本である。generic Terminal Launch 側の
producer `OperationId` と replay 契約は [generic terminal request](#generic-terminal-request) が正本で、Agent はそれを
複製せず自分の final identity だけを持つ。

request の `operation_id` は **client が発行した durable identity をそのまま載せたもの**である。daemon 側は request ごとに
別 identity を作らず、client 側の adapter も別 identity を作らない（TUI 側の pending pane との対応は
[3. TUI#同一 process の pending operation identity](03-tui.md#同一-process-の-pending-operation-identity) を正本とする）。

accepted / final のどちらの response も、body に次の 2 つを必ず持つ。`ResponseOutcome::Ok` の final は envelope に
operation identity を持たないため、body がその final を相関させる唯一の根拠になる。

| body field | 内容 |
|---|---|
| `operation_id` | この答えが属する producer `OperationId` |
| `semantic_digest` | 受理した intent の canonical semantic key の digest |

canonical semantic key は launch では `(WorkspaceId, SessionId?, profile ID?)`、exact resume では target 全体
（continuation・source・scope・worktree・runtime・adapter revision）から作る。key の書式は daemon と client が共有する
1 か所（`usagi-core` の client vocabulary）が持ち、digest は domain-separated hash で
[terminal input digest](#terminal-input-identity-と-cross-connection-replay) と衝突しない。

**launch** の答えについて、client は次を全て満たした答えだけを自分の pending operation の答えとして扱う。1 つでも
欠けた答えは safe correlation failure とし、`TerminalRef` を pending へ promote しない。exact resume の replacement を
受理する条件は identity ではなく daemon 自身の relation / lineage であり、
[3. TUI#明示 resume の検証](03-tui.md#明示-resume-の検証) が正本である。

| 検証 | 内容 |
|---|---|
| identity | envelope（accepted）と body の `operation_id` が request の identity と一致する |
| digest | body の `semantic_digest` が client 自身が request intent から計算した digest と一致する |
| 種別 | accepted は `completed: false`、final は `completed: true` を持つ（`completed: false` を final として受けない） |
| fence | `TerminalRef` が request の workspace / session scope に属する |
| relation | 通常の launch の答えは resume replacement relation を持たない |

cached replay は direct final と同じ body（同じ identity・digest・`TerminalRef`）を返し、client は経路によって検証を
省略しない。semantic key を持たない旧 durable record は digest を持たないため replay しても intent の一致を証明できず、
client は final として受けずに安全に失敗する。

## Codex structured capture request

`codex_session_capture` kind は、daemon が Codex の `SessionStart(startup)` command hook にだけ注入する
private request である。documented hook JSON の current `session_id` と、同じ process provision にだけ存在する
daemon-minted credential を持つ。client は runtime / session / provider / path を指定できず、daemon は credential
から exact live Codex runtime を逆引きして structured capture 境界へ渡す。成功 response は body を持たず、
provider ID を返さない。

credential の欠落・不一致・失効、hook event / JSON / provider ID の不正、runtime の非 live、永続化失敗は safe error
であり、metadata を作らない。request の native ID はこの capture の入力でだけ一時的に IPC を通り、通常の Agent /
session request、response、event、status projection、error detail には現れない。hook input の `transcript_path` は wire field
に変換せず、file を開かない。capture と durable resume の正本は
[Provider-native conversation resume](05-daemon.md#provider-native-conversation-resume) とする。

## agent phase report request

`agent_phase_report` kind は、daemon が起動した agent のライフサイクルフックだけが送る private request である。
field は closed vocabulary の `phase`（`ready` / `running` / `waiting` / `ended` / `exited`）と、同じ process
provision にだけ存在する daemon-minted credential の 2 つだけである。client は runtime / session / worktree /
path / provider を指定できず、daemon は credential から exact live runtime を逆引きする。成功 response は
body を持たない。

phase は wire に載る前に hook 側で検証する。usagi が配線した lifecycle event（`SessionStart` /
`UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `Notification` / `Stop` / `SessionEnd`）と phase の対応が hook input の
`hook_event_name` と一致しない報告、未知 phase、malformed JSON、credential 欠落は request を作らない。
`transcript_path` は wire field に変換せず、file も開かない。

daemon 側では credential の欠落・不一致・失効、runtime の非 live、malformed body、永続化失敗が safe error に
なり、phase を記録しない。この request は producer `OperationId` を持たない mutation なので、
[attempt deadline と reconnect budget](#attempt-deadline-と-reconnect-budget) の request class では
cross-connection retry の対象外である（report の欠落は次の hook 呼び出しで置き換わる）。反映の優先順位と
durable な写像は [Agent phase の投影](05-daemon.md#agent-phase-の投影) を正本とする。

## provider conversation resume request

`agent_inventory` は workspace root と全 managed session の live / interrupted history を同じ
`AgentInventory` として返す。live runtime item は完全な `AgentRuntimeRef`、
`AgentContinuationRef`、runtime state、optional source relation を持つ。resumable item は runtime ごとに
`available` と provider ID を含まない closed enum の safe reason を持ち、現 schema の record には
`AgentResumeTarget` を載せる。加えて client が interrupted history を provider 単位で表示するための
closed vocabulary だけを additive に載せる（`provider` = `claude` / `codex`、`last_known_phase` = safe phase enum）。
metadata を保存していない record では両 field を省略し、client は欠落を推測で埋めない。旧 record は
`target: null` / unavailable のまま読み、identity を推測しない。
item は durable operation timestamp と stable runtime ID で決定的に並ぶため、同じ scope の複数 history
や Claude / Codex の混在を別 item として保持する。現 schema に `complete` / retention watermark はなく、inventory の
欠落は `AgentContinuationRef`、TUI dismissal、slot の削除を認可しない。TUI open は terminal inventory の前後 snapshot と
この `AgentInventory` が coherent な場合だけ全量を適用し、partial / cross-RPC 不整合では pane restore 全体を retry する。
Agent history / exit history / dismissal の allocator・retention・GC は
[#526](../.usagi/issues/526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) の責務であり、この request は
削除 authority を返さない。

`ResumeAgent` は利用者が明示的に開始する provider conversation の再開である。payload は canonical
`operation_id` と inventory が返した `AgentResumeTarget` をそのまま持つ。target は次の public fence だけで
構成する。

| field | fence |
|---|---|
| `continuation` | provider conversation lineage |
| `source` | resume source record の opaque incarnation |
| `workspace_id` / `session_id?` / `worktree_id` | root または managed session の exact scope |
| `runtime_id` | source runtime incarnation |
| `adapter_revision` | capture と resume adapter の互換 revision |

provider-native ID、provider 種別、cwd、profile argv、environment、transcript、旧 `TerminalRef` は target
にも他の client payload にも含めない。client は target を加工せず返し、daemon が durable record と全 field
を完全一致で検証する。したがって CLI、TUI、MCP は provider ID、名前、path、PID から target を再構成しない。
成功時は daemon が新しく所有する `AgentRuntimeRef` / PTY incarnation の完全な `TerminalRef`、同じ
`AgentContinuationRef`、`source` から replacement runtime / terminal への relation を返す。これは旧 PTY の
stream `resume` や再 attach ではない。

同じ operation と同じ exact target の再送は daemon restart 前後で同じ final と relation を replay し、別 target
への再利用は `idempotency_conflict` になる。double click 等で別 operation が同じ exact target を再送しても、
durable source → replacement relation から同じ final へ収束し、source は一度だけ supersede する。scope、worktree、
runtime incarnation、adapter revision、lineage の不一致、live source、metadata 欠落、provider unavailable は spawn 前に
safe な typed failure となる。native ID は
inventory、IPC、hook、error、log へ出さない。provider capture、fence、redaction、new PTY spawn の正本は
[Provider-native conversation resume](05-daemon.md#provider-native-conversation-resume) とする。

CLI の `resume-exact`、TUI の exact resume port、MCP `session_resume` は同じ exact contract を使い、inventory は
CLI / TUI port と MCP `agent_resume_inventory` が共通 contract を使う。session ID / name だけを指定する resume request は受け付けない。

daemon restart、TUI 起動、workspace open 時の pane 復元は `ResumeAgent` を送らない。利用者による明示操作だけが request を作る。

Doctor の integration repair は同じ exact resume 境界を狭く拡張する。`diagnose_agents` は invoking binary が持つ
code-defined profile revision と、live runtime の launch snapshot revision を比較し、古い hook / MCP integration
だけを provider ID・argv・設定本文なしで返す。診断には exact resume metadata の準備可否も含み、1件でも準備できていなければ
`restart_agents` は停止前に全件拒否する。`restart_agents` は利用者へ表示した診断集合の exact runtime ref を再送し、その集合だけを
停止する非 retry mutation である。診断後に追加された Agent は停止せず、選択済み ref が差し替わった場合は全件停止前に stale として
拒否する。reported phase が `running` の runtime は `force` なしで全件 effect-before-zero の `busy` となる。generic terminal は対象外である。

停止後、client は daemon build policy を適用して seamless rollover を完了し、返された exact target を
`resume_agent_with_current_integration` へ渡す。この repair-only request は source の旧 adapter revision を fence として
保持したまま、active daemon の期待 revision と current adapter capability を検証し、provider / native session ID / scope /
lineage を変えずに hook・MCP provision だけを再解決する。通常の `ResumeAgent` は revision migration を許可しない。
旧 daemon がこの診断 vocabulary を実装していない場合、live Agent が無ければ通常 rollover を行う。live Agent がある場合は
一覧を返して停止を保留し、`--restart-agents --force` が同時に指定された場合だけ既存の cold restart を使う。この互換経路は
generic terminal も停止し得るが、再起動後も今回停止した runtime ID に対応する exact target だけを resume する。

## dispatch request

`dispatch` は managed session の既存 create lifecycle と Agent launch を合成する即時実行 request である。payload は producer-issued `operation_id`、workspace、session name、execution context から得た caller、排他的な worker selector（既存 `agent_id` または `runtime` と `model`）、prompt を持つ。daemon は session を reuse/create して available scope を確認してから、prompt を `initial_prompt` として launch する。成功 reply は Accepted outcome と `run_id`（operation ID）および fenced terminal を返す。同じ operation の再送は同じ outcome を返し、異なる intent は idempotency conflict である。

dispatch の operation key、caller↔worker binding、runtime generation、safe outcome も restart 時に hydrate される。同じ dispatch の retry は worker を再 spawn せず、保存済み outcome を replay する。

client は path、argv、queue/live mode、completion destination を指定しない。Agent identity と query は workspace ownership を
durable に持ち、handshake が fence した workspace の Agent だけを selector / list / get の対象にする。別 workspace の
`agent_id`、available でない session scope、agent selector の不整合、または未知 agent は safe typed error となり PTY を
spawn しない。新規 agent の runtime/model は daemon が launch 直前に current workspace allowlist と current executable
availability で再検証する。allowlist 外は `invalid_argument`、executable 不在は `unavailable` とし、どちらも PTY を spawn しない。

## generic terminal request

generic terminal の request vocabulary は `terminal` kind の `launch`、`inventory`、`attach`、
`resume`、`resync`、`input`、`resize`、`detach` である。launch は stable profile ID、
`WorkspaceId` / optional `SessionId` / `WorktreeId` の scope、geometry だけを送る。command、argv、
working directory、environment、secret は wire field ではなく、daemon が trusted profile から解決する。

attach は additive optional field として自分の viewport を載せる。terminal を共有する window の要求は
attachment と同じ寿命を持つため、attach 自体が要求を宣言し、daemon は最小値を再計算してから同じ排他区間で
snapshot を取る。したがって attach 応答の `geometry` は、その要求を織り込んだ確定値である。field を載せない
旧 peer は従来どおり `Resize` だけで要求する。

launch の response は完全な `TerminalRef` を返す。attach は snapshot、connection-owned subscription、
`(connection, client, terminal)` ledger が次に期待する `next_input_seq` を同時に返す。input、resize、detach は
その `TerminalRef` と subscription を必ず含める。`next_input_seq` は generation 1 への additive optional field
であり wire generation は上げない。field を返さない旧 daemon に対して client は connection epoch の一致判定へ
fallback する。

launch intent は producer 発行の `launch_operation`（`OperationId`）を additive field として持つ。UI の
`OpenTerminal` effect が決めた durable identity をそのまま wire へ載せるため、response が失われて client が
再接続・再送しても、daemon は同じ producer operation の durable record を replay する。response は
`terminal`、`launch_operation`、その答えが replay かどうかを示す `replayed` を返す。canonical intent digest は
trusted profile ID・fenced scope・geometry から作り、同じ `launch_operation` に異なる digest が来た場合は
`idempotency_conflict` として既存 terminal も capacity も変更しない。digest を持たない旧 record は intent の一致を
証明できないため replay せず、同じく conflict になる。`launch_operation` を持たない旧 peer の launch は従来どおり
daemon 発行の identity になり、その再送は安全な mutation retry ではない。owner shard / capacity claim 側の契約は
[5. daemon](05-daemon.md#owner-generation-runtime-shard-と-global-resource-allocator) を正本とする。
terminal command の effect は、daemon generation、terminal、workspace、optional session、worktree、
runtime ownership/state の全 fence を read-only で検証した後だけ実行する。resize はこの preflight から
PTY effect、geometry commit まで terminal actor の排他区間を保持するため、途中の exit/replacement は
割り込まない。PTY effect が失敗した場合は `unavailable` を返し、committed geometry を更新しない。

`resize` は要求であって命令ではない。1 つの terminal は複数 client から attach されるため、daemon は
attach 中の client の要求の最小値を PTY へ適用し、**応答の snapshot には確定した geometry を載せる**
（policy の正本は [5. daemon#共有 viewport（複数 client の geometry）](05-daemon.md#共有-viewport複数-client-の-geometry)）。
client は自分が要求した値ではなく、この応答と attach snapshot の `geometry` で screen を組む。
最小値が動いた後、まだ古い geometry の screen を持つ client の `resume` は `resync_required` になる。
output は `(start_offset, end_offset)` の連続範囲で表す。attach / resync / resize が返す snapshot の
payload は negotiated revision で決まり（[snapshot payload と revision](#snapshot-payload-と-revision)）、
どちらの revision でも `revision`（terminal 側の geometry/exit fence）・`geometry`・`output_offset`・
`exited` を持つ。

resume は `after_offset` が window より古い場合、`output_offset` より未来の場合、または
その client が最後に渡された geometry が現在の geometry と異なる場合に `resync_required` を返す。window 内の segment
途中を指す場合は、その offset から始まる suffix を返し、最初の `start_offset` は必ず
`after_offset` と一致する。client は `resync_required` 後に snapshot で画面を置換し、返された
`output_offset` から resume する。同じ古い cursor を再送しない。この `base_offset` は protocol
generation 1 revision 1 の additive field であり、revision 1 client は必須 field として検証する。
resume の response は raw suffix と `exited` だけを返し、snapshot（screen capture）を伴わない。

### snapshot payload と revision

daemon は terminal grid / scrollback の唯一の権威であり、terminal ごとに VT screen を 1 つ持って
受信 byte を feed し、resize で screen を reshape する。snapshot payload は generation 1 の
negotiated revision で決まる。

| negotiated revision | payload | offset | 意味 |
|---|---|---|---|
| 1 | `replay`（raw byte tail） | `base_offset + replay.length == output_offset` | 移行互換の legacy 経路。tail は最大 64 KiB |
| 2 | `screen`（semantic checkpoint） | `base_offset == output_offset`（tail 長 0） | `output_offset` 時点の完全な screen state |

daemon は generation 1 の `max_revision` を 2 として広告し、`ServerHello.capabilities` に
`terminal.screen-checkpoint.v1` を含める。client も `max_revision` 2 を広告し、共通 revision が 1 に落ちる旧 client には
従来どおり raw tail を返すため、両 revision が同じ daemon で同時に成立する。revision 2 の `screen` は schema version・
geometry・active buffer・primary（常に存在）と alternate（active のときだけ）の grid / scrollback /
oldest-row origin /
cursor / saved cursor / scroll region、interned style table、decoder の途中状態、application cursor mode、bracketed paste mode、mouse protocol の有効状態と coordinate encoding を持つため、reattach 後の最初の paste / ホイールから full-screen program へ同じ入力列を送れる。client は
checkpoint から screen を復元し、`output_offset` からの raw suffix を同じ parser へ feed する。
raw tail を blank parser へ流すことに起因する UTF-8 / CSI / OSC の切断は revision 2 では起こらない。

checkpoint 経路の判定は capability を真実源とし、client 側の収束先は次のとおりである。

| client | daemon | capability | 共通 revision | 収束先 |
|---|---|---|---|---|
| new | new | 有 | 2 | checkpoint から復元し suffix を feed |
| new | old | 無 | 1 | client が限定表示へ fail closed（raw tail を parse しない） |
| new | new | 無（広告漏れ） | 2 | 同じく限定表示（capability を真実源とする） |
| old | new | 有 | 1 | daemon が revision 1 の raw tail を返し、旧 client の既存挙動を保つ |
| new | new | 有 | 共通 range 無し | typed `protocol_mismatch` で handshake を拒否する |

限定表示の内容は [3. TUI#snapshot-negotiation-と-legacy-限定表示](03-tui.md#snapshot-negotiation-と-legacy-限定表示) が正本である。

snapshot は **client 側の view を作り直すだけ**であり、PTY の lifecycle には触れない。attach / detach / disconnect と
再 attach を挟んでも daemon は child を respawn せず、child PID と spawn 回数は不変である。attach / resync / resize /
exit 後の final snapshot は Agent 面と generic 面で同一の payload 契約（revision 2 では `screen` checkpoint と
`base_offset == output_offset`）に従う。

`screen` の生成は保持中の screen に比例するため、**offset だけが必要な経路は snapshot を取らない**。受理した
output chunk の journal 追記、`Resume` の liveness 判定、exited tombstone 一覧（`CompletedInventory` の
`base_offset` / `final_output_offset`）は retained window と exit status を読むだけの経路を使い、PTY chunk ごとや
tombstone ごとに screen capture を払わない。

snapshot の `geometry` と `screen.geometry` は常に一致し、片方だけ新しい frame は存在しない。resize は
preflight → PTY effect → geometry commit → screen reshape を terminal actor の排他区間で行い `revision` を
1 つ進めるため、checkpoint の生成前後に resize が割り込んでも client は `revision` の逆行として検出し、
old / new state を混ぜずに snapshot 再取得（retry / typed resync）へ落とせる。geometry 自体は client の
要求ではなく daemon 権威なので、snapshot が載せた `geometry` は拒否せず採用する。

daemon 側の bound は次のとおりで、いずれも既定 1 MiB frame と process の memory peak を守る。

| bound | 効果 |
|---|---|
| geometry の上限（`ROWS_MAX` / `COLS_MAX`） | 範囲外の geometry は `invalid_argument` で PTY effect も grid 確保も行わない |
| per-terminal cell budget | screen が超えたら古い scrollback から trim する（trim 行数を counter に計上） |
| process-local aggregate cell budget | 直前に増えた terminal を、他 terminal の現在の retention が残す範囲まで trim する |
| serialized checkpoint budget | checkpoint payload の古い scrollback を落として frame 内に収める。可視 grid だけでも収まらない場合は `resource_exhausted` で fail closed とし、部分的な screen を返さない |

`stale_target`、`ownership_unknown`、partial write を含む安全に証明
できない結果は typed error であり、client は local PTY を生成しない。

terminal input は daemon が PTY master に受理された byte 数を追跡し、operation の outcome として保持する。
同じ client の同じ `input_seq` と request identity を再送した場合は保存済み outcome を replay し、PTY へ再送しない。
connection を越える identity・ordering・replay は [terminal input identity と cross-connection replay](#terminal-input-identity-と-cross-connection-replay) を正本とする。

| PTY write outcome | input ack | retry contract |
|---|---|---|
| 全 byte を適用 | `Written` | 同一 operation の再送は `Cached(Written)` |
| 適用済み prefix が 0 byte の failure | `Failed` | effect がないため、新しい operation として安全に再試行できる |
| 1 byte 以上を適用後の failure / `WriteZero` | `Ambiguous { applied_prefix }` | 同一 operation の再送は `Cached(Ambiguous { applied_prefix })` とし、既適用 byte を暗黙に再送しない |

PTY write が `Interrupted` を返した場合、daemon はそれまでの `applied_prefix` を維持して残りを再試行する。
wire 型は既存の `applied_prefix` を使うため protocol revision の変更を伴わない。

TUI adapter は final `ResponseOutcome::Ok` の ACK body だけを検証し、`Written` だけを通常成功として投影する。
Input response の `Accepted` はpendingであってfinal ACKではないため、bodyが見かけ上 `Written` でもeffect unknownとして拒否する。`Failed` / `Ambiguous` と
それらを包む `Cached` も daemon が input sequence を消費した final outcome なので、client は sequence を進めるが
subscription は切らない。`Ambiguous.applied_prefix` は `1..=input.length` だけを受理し、未知 variant、0 / 範囲外 prefix、
過剰に深い `Cached` は effect unknown として fail closed にする。

terminal Input の protocol error は `side_effect: none` の場合だけerror codeをdefinitive failureへ写せる。
`partial_or_unknown` / `applied` / `operation_accepted` はcodeにかかわらずeffect unknownであり、「未配送」と表示せず
connectionを捨て、blind replayしない。

request の write を試みた後で EOF / transport failure になった場合、client は PTY effect の有無を証明できない。
この ACK-loss 経路は「未配送」へ変換せず delivery unknown と表示し、同じ bytes を自動再送しない。
ACK lossと`Ambiguous`のuncertaintyはreattach成功や後続`Written`でclearせず、複数件をbounded count + first/latestで
集約する。uncertaintyを解消できるのは
[cross-connection replay](#terminal-input-identity-と-cross-connection-replay) の durable outcome resolution と
session 破棄だけで、transport recovery や後続の成功では clear しない。後続の
fatal/transport errorはprior uncertaintyを隠さず、current stateと合成して投影する。

### terminal input identity と cross-connection replay

ACK を受け取れなかった terminal input の outcome を、**その request を運んだ connection が消えた後でも**照会・replay する
契約である。この節が cross-connection input identity・ordered replay・expiry の正本である。

#### identity の分離

5 つの identity を別物として扱う。混同すると、reconnect 後の照会が到達できない（ledger を connection に紐づけた場合）か、
同じ input が二度 PTY へ届く（順序番号を operation identity として使った場合）。

| identity | 発行者 | lifetime | 用途 |
|---|---|---|---|
| client incarnation（`ClientHello.client_id`） | client process | client process 1 回の起動 | daemon の durable per-client state（input operation ledger）の key |
| connection epoch | client（transport 交換ごとに増える client-local 値） | 1 本の transport | subscription の有効性判定。epoch が変われば全 subscription が無効 |
| subscription | daemon（`attach` の応答） | それを発行した connection | どの attachment が write してよいかの fence |
| `input_seq` | client | connection epoch に局所 | 同一 connection の detach / fresh subscription を跨ぐ順序番号。fresh connection で 0 に reset する |
| `input_operation`（`OperationId`） | client（input ごと） | daemon ledger の bound 内 | request retry / reconnect / reattach を越えて同じ logical input を識別する |

`client_id` は canonical resource ID（UUID）でなければならない。PID は再利用されるため、PID 由来の identity では
新しい process が別 process の operation を継承し得る。合成ルートは process ごとに 1 つの UUID を発行し、per-request lane・
terminal stream lane・poll lane のすべてで同じ値を申告する。これは workspace fence と同じ「同一 UID の協調する peer 同士の
一貫性 fence」であり、authorization boundary ではない。

`input_seq` を cross-connection の operation identity として使わない。逆に `input_operation` は epoch に依存しないため、
fresh connection が `input_seq` を 0 へ戻しても、未収束 operation の照合は影響を受けない。同じ connection の fresh
subscription は attach 応答の `next_input_seq` を採用し、ledger position を継続する。

#### wire

`terminal` kind に次を追加する。daemon は `terminal.input-operation.v1` capability を広告し、client はこの capability を
真実源として経路を選ぶ（negotiated revision だけでは判断しない）。

| 要素 | 形 | 意味 |
|---|---|---|
| `input` の `input_operation` | `OperationId?`（additive、省略可） | この input の durable identity。省略は ledger を持たない旧 client |
| `input_outcome` action | `{"terminal", "input_operation"}` | 記録済み final の read-only 照会。PTY へは何も書かない |
| `input_outcome` の応答 | `{"outcome":"final","ack":InputAck}` / `{"outcome":"unknown"}` | 記録があれば同じ final、無ければ typed unknown |

`input_outcome` は read-only なので、[request class](#attempt-deadline-と-reconnect-budget) 上は新しい connection で再照会
できる。`input` 自体は durable identity を持っても cross-connection retry の対象にしない。ACK loss の解消は「照会」であり
「再送」ではない、という一点に契約を寄せている。

migration は次のとおりで、いずれも fail closed である。

| 組み合わせ | 挙動 |
|---|---|
| 新 client + 新 daemon（capability 有） | `input_operation` を送り、ACK loss は `input_outcome` で収束させる |
| 新 client + 旧 daemon（capability 無） | `input_operation` を送らず、`input_outcome` も送らない。ACK loss は unknown のまま latch する |
| 旧 client（`input_operation` 無し）+ 新 daemon | 従来どおり connection-local な `input_seq` ledger で動作し、cross-connection replay は得られない |
| canonical な client incarnation を申告しない peer が `input_operation` を送る | `unauthenticated` で拒否する。ledger を scope できないため、後の「replay」が二度目の write になり得る |

#### daemon 側の ledger

daemon は `(client incarnation, input_operation)` を key に、**terminal registry 全体で 1 つの** bounded ledger を持つ。
terminal ごとに分けないのは、同じ operation identity を別 terminal へ再利用したとき、fresh write ではなく conflict として
検出するためである。lookup は attachment 検証より**前**に行う。ACK を落とした client は subscription も失っているので、
attachment を先に要求すると、まさに必要なときに outcome へ到達できない。

| 提示された operation | 判定 | 効果 |
|---|---|---|
| 記録済み・同じ terminal・同じ semantic digest | replay | `Cached(記録済み final)` を返し、PTY へ書かない。`input_seq` も進めない |
| 記録済み・別 bytes または別 terminal | `idempotency_conflict` | 何も書かない。別 target へ適用しない |
| 未記録 | 新規 | attachment・liveness・`input_seq` を検証してから 1 回だけ PTY へ書き、final を記録する |

semantic digest は `(terminal, bytes)` の SHA-256（component は length-prefix する）であり、daemon が request から導出する。
`input_outcome` は terminal identity だけを fence し、attachment・liveness は要求しない。したがって write path が閉じた
**exit 後**でも、その前に記録された final を返せる。記録が無い場合は `unknown` であり、error でも成功でもない。

ledger の bound は次のとおりで、超過は古い record から解放する。解放された record は `unknown` を返す。

| 次元 | 既定 |
|---|---|
| operation 数（process 全体） | 4096 |
| operation 数（client incarnation ごと） | 256 |
| 保持 payload byte 数 | 1 MiB |
| age | 5 分 |

age は daemon の retention clock で測る（terminal retention と同じ時計を使うため、test は同じ fake で決定的に駆動できる）。

daemon 側の 2 つの ledger は key と lifetime が異なる。混ぜると、reconnect のたびに最初の input が stale sequence として
拒否されるか、逆に古い operation へ到達できなくなる。

| ledger | key | 消える契機 |
|---|---|---|
| `input_seq` の期待値 | `(connection, client incarnation)` | その connection の終了（client 側の epoch reset と対になる） |
| operation final | `(client incarnation, input_operation)` | 上表の bound だけ。connection の終了では消えない |

#### client 側の ordering fence

effect unknown は表示だけでなく、**per-terminal producer queue の ordering fence** である。unknown な先頭 operation が
高々 1 件あり、それが収束するまで同じ terminal の後続 input を PTY へ送らない。送ってしまうと、まだ適用され得る先行 input
を追い越すか、その input が途中まで書いた command に後続の bytes が連結され得る。

| 状態 | 後続 input |
|---|---|
| fence 無し | そのまま送る。input ごとに新しい `input_operation` を発行する |
| fence 有り・queue に空き | 生成順で bounded queue に保持する（PTY へは届かない） |
| fence 有り・queue 満杯（既定 64 件 / 8 KiB） | typed backpressure で拒否する。黙って捨てず、順序も入れ替えない |

収束は次のように進む。fence 解消のたびに queue を生成順で流し、途中で再び unknown になれば残りはその後ろで順序を保つ。

| 照会結果 | 挙動 |
|---|---|
| `final`（`Written`） | fence を解放し、その input の uncertainty を撤回して queue を流す |
| `final`（`Failed` / `Ambiguous`） | fence を解放して queue を流すが、outcome は成功へ変換しない（`Ambiguous` は uncertainty として latch し続ける） |
| `unknown` | fence を latch する。ledger は忘れる方向にしか変化しないため再照会せず、blind resend もしない。解放には明示的な user abandonment / recovery policy が必要で、現行 UI では session 破棄だけがこれを解く |
| transport failure | fence と照会をそのまま維持し、reconnect 後に再照会する |

#596 の fresh connection が `input_seq` を 0 へ戻しても、未収束 operation と queue は消さない。reattach は streaming の
回復であって、失われた ACK の収束ではないからである。

`inventory` は `WorkspaceId` / optional `SessionId`（None=root）/ `WorktreeId` の scope を送り、
その scope に**完全一致**する daemon 所有 runtime を列挙する。daemon は generic terminal owner と
Agent owner の両方に問い合わせて結果を merge するため、応答には**generic terminal と Agent terminal の
両方**が含まれる。各エントリは完全な `TerminalRef`、`kind`（`terminal` / `agent`）、`live`（現 daemon
generation が所有し attach 可能か）だけを持ち、argv・environment 値・secret・provider transcript は
含めない。`exited`・reconcile 中・orphan の runtime は `live: false` として返り attachable にはならない。
client はこの列挙で発見した live runtime にだけ、その `TerminalRef` で fenced に attach する
（名前や path から terminal を推測しない）。workspace open 時の pane 復元でこの列挙を使う（[3. TUI](03-tui.md#workspace-open-時の-pane-復元) を正本とする）。

daemon restart 後も `inventory` は retained shard から復元した generic terminal record を同じ scope と
`TerminalRef` のまま返す。ただし旧 daemon の PTY master は復元しないため、未終端 record は
`identity_unknown`、`live: false` となる。旧 ref の attach、resume、resync、input、resize、detach は
typed safe error となり、別 terminal の PTY effect や暗黙の replacement spawn を起こさない。restart 時の
永続化・破損時の扱いは [5. daemon](05-daemon.md#daemon-data-directory) を正本とする。

## exited tombstone visibility

`inventory` の liveness 契約（`live: true` は attach 可能な running のみ）は変えない。exited した
terminal/Agent の tombstone へ fresh TUI が到達するための query/command を additive に追加する
（正本は [5. daemon の terminal ownership](05-daemon.md#terminal-ownership)）。terminal request kind に次の
3 action を加える。scope は generic の `inventory` と同じ `WorkspaceId` / optional `SessionId`（None=root）/
`WorktreeId` の完全一致である。

| action | payload | response |
|---|---|---|
| `completed_inventory` | `scope` | `{"entries": [CompletedTerminalEntry]}` |
| `observe` | `terminal`（完全な `TerminalRef`）, `expected_revision` | `{"visibility", "applied", "conflict"}` |
| `dismiss` | `terminal`, `expected_revision` | 同上 |

`completed_inventory` は generic owner と Agent owner の両方の **exited** record だけを merge し、各 entry に
authoritative な workspace-global visibility を付けて返す。running / reserved / reconcile 中 / reclaimed は
含めない（これらは `inventory` の `live` 契約が扱う）。`CompletedTerminalEntry` は次を持ち、argv・
environment 値・secret・provider transcript は含めない。

| field | 意味 |
|---|---|
| `terminal` | tombstone を一意に決める完全な `TerminalRef`（retention identity も兼ねる） |
| `kind` | `terminal` / `agent` |
| `exit_status` | PTY 終了時に記録した exit status |
| `base_offset` / `final_output_offset` | bounded final replay window の locator（`[base_offset, final_output_offset)`） |
| `visibility` | 後述の workspace-global visibility（`state` と `revision`） |

visibility は client-local ではなく、同じ local user の **workspace-global durable state** であり、daemon が
唯一の authority として全 client connection を収束させる。state は `unobserved < observed < dismissed` の
monotonic lattice で、`revision` を伴う。

- `observe` / `dismiss` は expected `revision` を伴う compare-and-swap である。state が上がるときだけ
  `expected_revision` を照合し `revision` を増やす（`applied: true`）。
- 要求した state が現在値を上げない同値・下位 retry は idempotent な no-op で、`applied: false` / `conflict: false`
  と現在の authoritative snapshot を返す。late `observe` は `dismissed` を下げない。
- state を上げる必要があるのに `expected_revision` が stale な場合は `conflict: true` と authoritative snapshot を
  返す。client はそれを max-state へ merge し、返った `revision` で再送する。
- 別 exact `TerminalRef` の visibility は完全に独立する。out-of-order / duplicate な write は completed entry を
  復活させない。`dismiss` は visibility だけを変え、terminal や process には触れない。

client は `completed_inventory` が返した exact `TerminalRef` の visibility だけを操作し、名前・pane・
continuation で別 incarnation へ fallback しない。TUI の projection 契約は
[3. TUI](03-tui.md) を正本とする。aggregate retention / GC は
[#526](../.usagi/issues/526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) の責務である。

## owner generation routing

planned restart 中は old generation が draining のまま自分の PTY を持ち続け、new generation が
active として新規 launch を受ける。この間 client は「current endpoint 一つ」では old generation の
terminal に到達できず、推測で new active へ送れば別 terminal に effect を与える。client 側の
routing 契約は `usagi-core` の `usecase::owner_routing` が正本であり、次の表がその全体である。

| request | 配送先 |
|---|---|
| workspace / session / issue / Agent launch / generic terminal launch 等の control operation | active generation |
| scope inventory（`inventory` / `completed_inventory`） | trusted な全 generation。完全な `TerminalRef` で merge / dedup する |
| attach / resume / resync / input / `input_outcome` / resize / detach / `observe` / `dismiss` | request が持つ完全な `TerminalRef.daemon_generation` の owner endpoint |
| unknown / retired / forged generation | typed `stale_target`。current endpoint や同名 terminal へ fallback しない |

client は `ClientHello.capabilities` に `owner-generation-routing.v1` を広告し、daemon も
`ServerHello.capabilities` で同じ capability を広告する。daemon はこの広告を rollover 開始の
前提条件として読み、条件を満たさない場合は authority handoff の前に typed refusal で止める
（[5. daemon の rollover 前提条件](05-daemon.md#rollover-の-routing-前提条件)）。

### trusted endpoint の解決

generation endpoint は daemon が書いた record からだけ解決する。client が socket path を指定できる
API は存在せず、caller が渡せるのは `DaemonGeneration` だけである。解決は次の 2 通りで、いずれも
daemon が書いたものだから trusted である。

- `generations.json` がある場合はそこにある retained generation の role と endpoint を使う。
  `standby` は活性化まで private、`retired` は client が tab を回収してよい verified absence として
  除外する。
- `generations.json` が無い（rollover していない daemon）場合は `current.json` が全体の authority で
  あり、単一 active generation として振る舞う。

解決した endpoint は接続前に「その generation 自身の private directory 内の socket」であることを
再検証する。record が別 generation の socket を指していれば接続せず拒否する。重複 generation、
複数 active、active を指さない `current` は inconsistent として fail closed にする。

### merge と partial answer

scope inventory は generation ごとに問い合わせ、次の 2 つの fence を通った entry だけを採用する。
answering generation 以外の `daemon_generation` を持つ entry（別 daemon の terminal を名乗る）と、
要求 scope 外の entry は捨てる。残りを完全な `TerminalRef` で dedup し、同じ順序へ正規化するため、
同じ observation を何度 merge しても各 terminal は一度だけ tab へ投影される。

answer が無かった generation は `unreachable` として記録し、absence には変換しない。tracked terminal
の presence は次のように決まる。

| 観測 | presence |
|---|---|
| owner が live として列挙した | `live` |
| owner が answer したが列挙しなかった / `live: false` | `gone`（tab と endpoint record を回収してよい） |
| owner が answer しなかった（timeout / transport failure / 未問い合わせ） | `reconnecting`（last-known tab を保持する） |
| generation が trusted set から消えた（verified retirement） | `gone` |

partial inventory を cold-restart の interrupted projection へ変換しない。

### generation ごとの connection と cursor

connection、negotiated capability、output cursor は generation ごとに独立して保持する。active locator
が変わっても draining generation の link は破棄されない。transport failure はその generation の socket
だけを落とし cursor は残すため、再接続は replay ではなく cursor からの resume になる。cursor は
monotonic であり、遅れて届いた frame が tab を巻き戻すことはない。link を回収するのは verified
retirement（trusted set からの消失）だけである。同じ generation の endpoint 表記が変わった場合は
再利用せず新しい link として接続し直す。

### client 側の lane 配線

出荷 client の lane は **owner generation を key に持つ**。lane を開くときだけ trusted endpoint を解決し、
すでに開いている lane は map の key 一致だけで選ぶ。

| lane | 宛先の決め方 |
|---|---|
| control / launch（per-request client、CLI・MCP・TUI の control 操作） | current locator が指す active generation。cold start（daemon 未起動）の bootstrap もこの経路が持つ |
| attach / input / `input_outcome` / detach | request が持つ `TerminalRef.daemon_generation` の owner lane |
| `resume` / `resize`（stateless poll lane） | 同上。owner ごとに独立した lane を持つ |
| scope inventory（render thread と background pump の両方） | trusted な全 generation へ fan-out し、merge した結果を使う |

owner が **active role** に解決されたときの接続は current locator 経路そのもので、published locator の検証・
bootstrap・exact-owner process fence が全部そのまま効く。**draining role** に解決されたときだけ、その generation
自身の private socket へ直接接続する。draining generation は cold start も再 publish もされないため、応答が無い
ことを理由に daemon を起動すると owner とは別の daemon ができてしまうからである。この経路では handshake 後に
「相手が名乗った generation が要求した owner と一致すること」を確認し、一致しなければ lane を渡さず拒否する。

generation が 1 つしか published されていない build では owner は常に active に解決されるため、接続先・fence・
観測される挙動は current locator 経路と同一である。

### registry 読み取りの契機

`generations.json` と `current.json` は file であるため、request ごとに読むと IPC hot path に directory
traversal と file 読み取りが乗る。client は trusted snapshot を cache し、次の 3 つの契機でだけ読み直す。

| 契機 | 理由 |
|---|---|
| 最初の解決 | 解決対象の snapshot がまだ無い |
| 解決に失敗した | snapshot が、その owner を publish した handoff より古い可能性がある |
| 解決した endpoint へ接続できなかった / 相手が別 generation を名乗った | snapshot が指す endpoint が現実と食い違っている証拠。snapshot からは見えない |

refresh 後も解決できない owner はそのまま typed `stale_target` で拒否する。1 度の refresh で足りなかった場合、
2 度目の失敗が答えである。directory を読めなかった場合は直前の snapshot を保持したまま拒否し、live owner を
unaddressable にしない。

### capability と routing の適用範囲

client は generation の数にかかわらず常にこの routing を通す。`owner-generation-routing.v1` を広告しない
daemon と接続しても routing を切る経路は存在しない。generation が 1 つのとき owner は active に解決され、
その接続は capability の有無に関係なく成立するからである。この capability は **daemon が rollover を開始して
よいかどうかの前提条件**としてだけ読まれ、満たさない場合は authority handoff の前に typed refusal で止まる
（[5. daemon の rollover 前提条件](05-daemon.md#rollover-の-routing-前提条件)）。

## Unix transport

Unix socket は daemon 専用 adapter が管理する。endpoint は private data directory の generation
directory に作り、bind 成功後に current locator を atomic publish する。directory は `0700`、socket と
locator は `0600` で、所有 UID・mode・symlink でないことを discovery と accept の両方で検証する。
`SecureUnixListener::bind` は endpoint bind と current publish を一つの処理で行う。両者を分けたい呼出元は
`bind_private` で endpoint だけを用意し、authority を移す時点で `publish_current` を呼ぶ。publish は locator lock 下で
socket の inode identity を再検証するため、bind 後に endpoint が置き換わった generation は current になれない。
`publish_recovered_locator` は crash recovery 専用で、listener を所有しない process が committed handoff を roll forward
するときにだけ使い、endpoint が当該 generation の private directory 内の安全な socket であることを再検証する
（[5. daemon の cross-process generation authority](05-daemon.md#cross-process-generation-authority)）。

active / standby の accept loop は peer UID の一致を確認した後、[pre-handshake admission と deadline](#frame-と-handshake)を適用する。
credential check は admission permit と worker spawn より前であり、不一致 peer は従来どおり protocol byte を読まずに close する。
capacity refusal も accepted descriptor 以外を複製せずに close するため、同一 UID の peer が incomplete hello を保持しても
daemon の thread / FD 使用量は pre-handshake 上限と全 client worker 上限に比例して有界である。retirement 用 descriptor を
複製できなければ、その connection は worker を起動せず fail closed とする。

private directory は、検証済みの trusted parent directory の直下に `0700` を mkdir syscall へ指定して作る。
そのため process が mkdir と事後 chmod の間で停止しても group / other に公開された directory は残らない。
既存 path は symlink でない directory、effective UID、pathname と opened directory fd の device / inode を
検証する。abnormal exit が残した permission bit が `0700` の部分集合である owner directory だけを exact inode の
まま `0700` へ修復できる。所有者不一致、group / other bit を持つ directory、non-directory、path replacement は
修復せず拒否する。同時 first boot は同じ規則で作成済み path を再検証するため、一方の transient state を unsafe
directory として採用しない。first-use の作成・修復は検証済み parent directory fd の lock 下で直列化する。
selected runtime data directory が複数 component 未作成の場合は、effective UID owner かつ group / other
writable でない最深の既存 directory（または root-owned exact `01777` temporary anchor）から各 component を
同じ `0700` 規則で順に作成・修復する。intermediate component が restrictive umask と crash により mode `000` を
残しても、trusted anchor まで巻き戻して exact inode を修復してから traversal を再開する。
ただし root-owned exact `01777` temporary anchor は caller 所有ではなく parent fd lock を取得できないため、その直下の
最初の component だけは atomic `mkdir(0700)` と sticky / trusted-anchor 検証で競合を解決する。作成済み path は同じ
invariant で再検証し、それ以降の owner directory component は parent fd lock 下で直列化する。

socket discovery と process lifecycle discovery は別の fence を使う。`current.json` は generation endpoint の owner
を、`daemon.json` は daemon process の `(pid, process_start_identity, started_at)` を示す。client lifecycle は PID
の存在だけで record を active / stale と判断しない。OS process-start identity が保存値と一致する場合だけ active、
PIDが存在しない場合とprocess-start identity mismatchはreclaimable staleとし、legacy identity欠落と観測失敗は
unverifiedとする。exact owner signalはidentity一致時だけ許す一方、到達不能endpointのcleanupは`daemon.lock`取得と
record全体の再照合を別のreclaim authorityとして使う。lockまたは再照合を証明できなければlocatorとrecordを保持する。
exact owner signal と lifecycle cleanup の順序は
[5. daemon](05-daemon.md#daemon-process-lifecycle) を正本とする。

`ServerHello.daemon_process` は server の自己申告であり、単独では lifecycle authority にならない。client は established
`UnixStream` から Linux `SO_PEERCRED.pid` / macOS `LOCAL_PEERPID` を取得し、peer PID、OS process-start identity、
`daemon.json` の exact record、`current.json` の generation、active `ServerHello` generation と
`daemon.owner-identity.v1` capability が全て一致した場合だけ接続を owner として受理する。peer credential の取得不能、
nonce / PID / process-start / record / generation / hello の不一致、timeout、未認証 error frame は request を一件も送らず
effect zero で fail closed する。同一 UID の置換 socket が正しい record を echo しても OS peer PID が異なるため authority
を得ない。

current locator の publish と retire は owner-only の `current.lock` で直列化する。listener owner は
自分が publish した `(generation, endpoint)` と現在の locator が一致する場合だけ `current.json` を
unlink し、自 generation 固有の socket だけを回収する。retire は socket の不在を先に確定し、その後だけ exact locator を
unlink するため、locator の消去が endpoint cleanup の commit fence になる。したがって stale generation の遅延 retire / Drop が
replacement generation の locator または socket を削除することはない。planned generation end は accept loop の
停止・join 後にこの retire を完了し、client discovery を `NotFound` へ戻す。
`current.lock` を含む lifecycle lock node の secure create / reopen と pathname identity の契約は
[5. daemon data directory](05-daemon.md#daemon-data-directory) を正本とする。

lock を保持した locator writer は writer ごとに一意な private temporary file を `create_new` と
`O_NOFOLLOW | O_CLOEXEC` で開き、file descriptor を `fchmod(0600)` した後に regular file・所有 UID・
exact mode・`nlink == 1` を検証する。JSON 全体の write と file fsync 後にも同じ fd を再検証し、その場合だけ
`current.json` へ atomic rename する。rename 後は final path を `O_NOFOLLOW | O_CLOEXEC` で開き、同じ inode、
regular file、所有 UID、exact mode、single link であることを検証する。discovery も final path を secure-open した
同じ fd からこれらの invariant を再検証して読むため、symlink、hardlink、non-regular node を拒否する。

replacement publish は secure-open した old locator の exact bytes を別の private single-link temporary に保持する。
rename 前の create / write / sync / verify / rename failure は既存 locator を置換せず、writer 所有 temporary を回収する。
rename 後の final verify が失敗した場合は、final path がまだ prepared inode と一致する場合だけ `current.lock` を保持したまま
old bytes を atomic rename で復元する。old locator が無かった場合も同じ identity の new locator だけを消去する。final path が
既に別 inode へ置換されていれば replacement を上書きせず、writer 所有 temporary をすべて回収して fail closed とする。
したがって caller が error を受けた ordinary failure では old locator の bytes と接続可能性を維持し、concurrent replacement を
破壊しない。bind 側は new generation socket と `.sock.bind` を安全に回収できる。rollback image は hardlink を使わないため、
公開中の old locator の `nlink == 1` を崩さない。rollback rename / unlink 後の parent directory fsync も best-effort で行う。

process crash が残した旧 fixed temp や別 writer の temp は publication 時に推測削除せず、一意な次の temp で継続する。
rename と final verify の完了後に行う parent directory fsync は best-effort であり、commit 済み publication を失敗として
報告しない。atomic publication は generation の新旧順序を推測せず、旧 generation の locator や socket を回収する根拠にも
使わない。

publish が失敗した generation は、まだ owner object が構築されていなくても自 socket、`.sock.bind`、当該 writer の
temporary、新 locator の rollback を試み、rollback failure も error として返す。bind 成功後は listener fd と独立した
exact endpoint cleanup token を daemon owner が保持する。startup failure、accept-loop panic、join / Drop / retire failure で
listener ownership を失っても token から socket-first cleanup を再試行し、完了前に lifecycle record を消去しない。
stale recovery の singleton-lock / exact-record fence は [5. daemon process lifecycle](05-daemon.md#daemon-process-lifecycle) を
正本とする。

client discovery は read-only であり、daemon directory を作成しない。未起動時の polling client が mkdir と chmod の途中を
startup owner に観測させることはない。stale recovery が generation nodes を走査する場合は `generations/` root 自体も
owner-only directory かつ symlink でないことを先に検証し、daemon directory 外の socket を回収対象にしない。

accept 時は OS peer credential の UID が daemon UID と一致しなければ、protocol byte を読む前に接続を
閉じる。control 用の endpoint 解決は active locator だけであり、generation directory 外を指す endpoint に
接続することはない。owner generation を名指しした terminal request だけが draining generation の endpoint を
解決でき、その解決も daemon が書いた record と当該 generation の private socket 検証を経る
（[owner generation routing](#owner-generation-routing)）。

cross-process standby handoff は
[#516](../.usagi/issues/516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) が、
owner-generation runtime shard は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) が
提供し、client 側の owner routing は本節の契約として実装済みである。shipping の `serve` は自分の
generation を durable registry の active として登録するため、`generations.json` は production に存在する
（[5. daemon の first activation](05-daemon.md#first-activation)）。shipping の `daemon restart` は live runtime が
あれば standby を stage し、old active へ [`rollover` request](#daemon-rollover-request) を送って gated handoff を
起動する。shipping の replacement は 1 本の durable operation に集約され、old active が IPC request から gated
handoff を駆動する（[5. daemon の planned replacement](05-daemon.md#planned-replacement)）。

## client の失敗処理

TUI、CLI、MCP は共通 daemon client port を通して managed session と terminal の要求を送る。接続失敗、
protocol error、ownership unknown は local managed PTY や local session mutation への fallback を許可しない。

CLI・MCP・TUI の per-request 経路（managed session / PR snapshot / metrics / agent launch・resume / user decision /
generic terminal launch・inventory）は共通の resilient client を通り、surface policy を
[attempt deadline と reconnect budget](#attempt-deadline-と-reconnect-budget) として実効化する。したがって daemon が停止しても
各 attempt は policy 時間内に typed unavailable で戻り、event loop を握る同期 request が TUI の draw / input / quit、
CLI の exit、MCP の response loop を無期限停止させない。retry の可否は同節の request class 判定が正本であり、
`OperationId` を持つ mutation を再送するときは元の operation identity を保持する。generic Terminal Launch は現行 wire に
producer `OperationId` がないため、この durable retry 契約の対象外である。attach 済み terminal の stream lane（attach /
resume / resync / input / resize / detach）は connection-local な subscription を持つため reconnect する client には
載せず、代わりに connection を保持したまま request ごとに deadline を張り直す
（[terminal lane の per-request budget](#terminal-lane-の-per-request-budget)）。budget 超過は下記の `unavailable` →
backoff reattach で扱う。

TUI が daemon の出力・exit を観測する 2 本の lane（attach 済み terminal の `resume` と、detach 済み background tab のための
scope 単位 `inventory`）は、いずれも描画スレッドの外で、上記の attempt deadline に加えて **client 側の bounded cadence**
と backoff を持つ。したがって idle な TUI が生む request rate は frame rate にも pane 数にも比例せず、遅い / hung / unavailable な
owner は当該 lane を後退させるだけで draw / input / modal / quit を止めない。cadence、fence（exact ref・scope・connection epoch・
要求時 cursor）、bounded な queue と 1 frame あたりの適用数は
[3. TUI#背景 observation lane](03-tui.md#背景-observation-lane) が正本である。TUI は stream sequence、resource revision、terminal output offset を別々に保持し、gap や
epoch の不一致では output を継ぎ足さず、snapshot resync を要求する。

terminal の `unavailable` は TerminalSession の connection-local subscription の喪失として扱う。TUI は
100ms から 2s 上限の指数 backoff 後、元の完全な `TerminalRef` に `attach` して atomic snapshot と
新しい subscription を取得する。transport EOF はclient connectionをdropして次回に開き直すが、
response bodyのlocal decode failureは同じclient connectionとinput ledgerを保持する。成功後は snapshot の
`output_offset` から `resume` し、backoff と connection-local input sequence を、client-local connection epochが
変わった場合だけresetする。同じconnectionでのsnapshot reattachは subscriptionが変わっても attach 応答の
`next_input_seq` を採用する。`stale_target`、`ownership_unknown`、exited は retry 対象ではなく、detach / tab
close も pending retry を解除する。どの失敗経路も replacement launch を行わない。

### stream connection の共有と subscription の無効化

daemon は subscription を `attach` した connection に所有させ、connection が終わるとその connection の
attachment をすべて解放する。input ledger（`input_seq` の期待値）も connection ごとの client identity に
紐づく。したがって 1 本の connection を複数 pane で共有する client では、connection の入れ替えが**その
connection 上の全 subscription を同時に無効化**する。

server は transport EOF を観測した connection worker から socket を先に解放し、connection-local な
subscription / input ledger の削除は daemon-owned cleanup worker へ渡して直列化する。ledger の走査が長時間
稼働した terminal の owner lock と競合しても、切断済み connection の reader / writer / retirement descriptor を
保持しない。cleanup queue 自体も全 client worker 上限と同じ容量に制限し、consumer が owner lock を待つ場合でも
queue memory と送信待ち worker の双方を有界にする。daemon shutdown は accept と全 connection worker を止めた後に cleanup queue を drain してから owner
runtime を破棄するため、非同期化しても connection-local state を取り残さない。

| client 側の観測 | 共有 connection | 全 subscription | 次に送るもの |
|---|---|---|---|
| 完全に受信した error response（`resync_required` / `stale_target` など）、decode できない `Ok` body、非終端の `Accepted` | 保持する | 有効なまま | 当該 pane の resync / typed failure だけ |
| transport 破断（EOF、frame 破損、write 失敗） | drop して次回に開き直す | 無効 | 各 ref の `attach` を、その ref の `resume` / `input` より先に |

無効化された subscription で `input` を送っても daemon は attachment を見つけられず、`stale_target` で effect zero に拒否する。
client はそれを送る前に `attach` し直すため、recovery 後の最初の入力も一度だけ書かれる。old connection の
subscription に対する `detach` は送らない（daemon がすでに解放しているため、現在の connection では
未知の subscription として `stale_target` になるだけである）。ある `TerminalRef` に対する新しい `attach` の後に届く superseded
subscription の `detach` も、その新しい attachment を解除しない。

client 側の epoch の持ち方と pane ごとの再 attach 順序は [3. TUI#connection epoch と subscription 無効化](03-tui.md#connection-epoch-と-subscription-無効化) を正本とする。

terminal input は Live な connection-owned subscription がある場合だけ送る。非 Live、subscription 不在、または
request を書く前の definitive failure は typed failure であり、client は success として捨てず未配送 feedback を表示する。
request write 後の transport / ACK loss は effect unknown として区別し、未配送と断定しない。どちらも再接続まで入力を
queue / replay せず、unknown inputをblind retryしない。

MCP の dispatch request は `DispatchTool` action として送る。daemon が session upsert、agent/run/binding
の解決、inbox の読み書きを行い、MCP は durable state を直接読んだり書いたりしない。完了・失敗は worker
の current run と binding が一意に一致するときだけ配送し、不一致は completion fence と同じ fail-closed
方針で no-op にする。
