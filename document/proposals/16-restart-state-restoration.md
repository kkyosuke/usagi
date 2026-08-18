# 16. daemon restart / crash 後の作業状態の復帰

> [設計提案一覧](README.md) ｜ 関連仕様: [daemon](../05-daemon.md) ｜ [TUI](../03-tui.md) ｜ [daemon IPC](../04-ipc.md) ｜ 関連提案: [PTY crash 継続](07-pty-crash-continuation.md)

daemon が cold restart（`daemon restart --force` / `stop` → `start`）または crash / `SIGKILL` / OS 再起動から
復帰したとき、利用者が **開いていた作業面をそのまま取り戻せる**ようにする設計である。

現在の contract では、seamless rollover（[planned replacement](../05-daemon.md#planned-replacement)）が成立する
経路だけが PTY を維持し、それ以外の復帰では PTY が失われる。本書は **PTY を継続せずに**、失われる作業面を
どこまで durable に取り戻せるかを設計する。PTY 自体の物理的な継続は
[7. PTY crash 継続](07-pty-crash-continuation.md) の領分であり、本書はそれと独立に進められる。

## 目次

- [目標と非目標](#目標と非目標)
- [現状で失われるもの](#現状で失われるもの)
- [機構](#機構)
  - [terminal continuation と ResumeTerminal](#terminal-continuation-と-resumeterminal)
  - [durable screen checkpoint](#durable-screen-checkpoint)
  - [workspace restore plan](#workspace-restore-plan)
- [復帰しないもの](#復帰しないもの)
- [却下した代替案](#却下した代替案)
- [段階と issue 分割](#段階と-issue-分割)
- [PTY broker との関係](#pty-broker-との関係)

## 目標と非目標

| | 内容 |
|---|---|
| 目標 | crash / cold restart の後、**何が動いていたか**が tab として戻る（Agent 会話と generic terminal の両方） |
| 目標 | 中断された runtime の **最後の画面**を read-only で読める |
| 目標 | 復帰が **1 操作**で始められる（tab ごとの手作業を必須にしない） |
| 非目標 | PTY master fd の継続。crash 後に同じ child へ input を送ること |
| 非目標 | 「継続した」と偽ること。live でないものを live として描かない |
| 非目標 | 明示操作なしの自動 resume。既存の[明示 resume 契約](../03-tui.md#interrupted-agent-の-tab-投影と明示-resume)を維持する |
| 非目標 | credential・reported phase・in-flight input の hydrate（[復帰しないもの](#復帰しないもの)） |

「復帰」は **同じ scope に新しい runtime を作り、旧 runtime を history として読めるようにする**ことであり、
旧 process を生き返らせることではない。この区別は label と本文の両方で利用者に見えていなければならない。

## 現状で失われるもの

restart / crash をまたいで **既に durable な**ものは次のとおりで、本書はこれらを変更しない。

| 対象 | 正本 |
|---|---|
| managed session の lifecycle・operation journal・trusted repository root | [durable operation](../05-daemon.md#durable-operation) |
| Agent conversation lineage（`AgentContinuationRef`）と provider-native resume 情報 | [agent ownership](../05-daemon.md#agent-ownership) |
| Agent tab の表示順・target ごとの選択（`agent-tabs.json`） | [pane 復元](../03-tui.md#workspace-open-時の-pane-復元) |
| exited terminal の final tombstone・replay window・workspace-global visibility | [final retention と aggregate GC](../05-daemon.md#final-retention-と-aggregate-gc) |
| PR inventory、dispatch registry、supervisor run、operation ledger | [daemon data directory](../05-daemon.md#daemon-data-directory) |

失われるのは次の 3 つである。本書の機構はこの 3 つに 1 対 1 で対応する。

| # | 失われるもの | 原因 |
|---|---|---|
| G1 | **generic terminal がまるごと消える** | crash 後の未終端 record は `identity_unknown` へ reconcile され、`Exited` ではないので [completed inventory](../04-ipc.md#exited-tombstone-visibility) に現れない。interrupted tab の投影は Agent 専用であり、generic terminal は lineage identity を持たないため、TUI は tab も history も再構成できない |
| G2 | **中断された runtime の最後の画面が読めない** | VT screen は terminal registry が process-local に持つ authority であり、durable な形で残らない。interrupted tab の本文は 1 行の safe reason だけになる |
| G3 | **復帰が per-item の手作業** | 復帰操作は tab ごとの `Ctrl-O r` / `session resume <name>` しかない。session と tab が増えるほど、restart 後に同じ操作を人が繰り返す |

G1 が最も重い。`login-shell` terminal は daemon が trusted profile と fenced scope から起動しているため、
**daemon の側に再構成に必要な情報が全部ある**にもかかわらず、それを指す identity が無いために捨てられている。

## 機構

### terminal continuation と ResumeTerminal

generic terminal に、Agent と対称な lineage identity を与える。

| Agent 側（実装済み） | generic terminal 側（本提案） |
|---|---|
| `AgentContinuationRef`（conversation lineage） | `TerminalContinuationRef`（terminal lineage） |
| `AgentResumeSourceId`（runtime incarnation ごと） | `TerminalResumeSourceId` |
| `AgentResumeTarget` | `TerminalResumeTarget` |
| `ResumeAgent` request | `ResumeTerminal` request |
| `resumed_from` / `superseded_by` relation | 同じ relation を同じ atomic snapshot に保存する |

- continuation は daemon が lineage ごとに 1 度だけ発行し、source ID は runtime incarnation ごとに発行する。
  どちらも restart を越えて durable であり、新しい lineage へ再利用しない。
- **cwd・program・argv・環境は client から受け取らない**。daemon が保存済み durable record の
  profile ID と fenced scope（workspace / optional session / worktree）から、
  [terminal launch environment](../05-daemon.md#terminal-launch-environment) の解決を **やり直す**。
  したがって resume は「保存した path を再利用する」経路ではなく、通常 launch と同じ trust boundary を通る。
- 検証は `ResumeAgent` と同じ形で行う。source は non-live（interrupted / exited / reclaimed）でなければならず、
  同じ continuation の live / reserved runtime、同じ source への in-flight resume、scope / profile revision の
  不一致、metadata 欠落は spawn 前に typed に拒否する。
- 書き込み順序は launch の L1..L5
  （[owner-generation runtime shard と global resource allocator](../05-daemon.md#owner-generation-runtime-shard-と-global-resource-allocator)）
  をそのまま使う。producer `OperationId` と target 全体を semantic key にするため、duplicate click・reconnect・
  restart 後の replay は同じ final へ収束し、新しい spawn も capacity reservation も作らない。

**resume で戻るのは「同じ scope の新しい login-shell」だけである。** shell の中で走っていたコマンド、
その shell の履歴・環境の途中状態は復元しない。旧 runtime は
[durable screen checkpoint](#durable-screen-checkpoint) が持つ最後の画面として read-only に残る。

### durable screen checkpoint

daemon は既に terminal ごとの VT screen の唯一の authority であり、attach / resync 用の
[semantic screen checkpoint](../04-ipc.md#snapshot-payload-と-revision) を作れる。これを **crash に耐える形**に
する。目的は表示であって resume ではない。

| 項目 | 決定 |
|---|---|
| 書き手 | terminal owner generation。自 generation の checkpoint document だけを書く |
| 書く場所 | shard とは別の immutable object `screens/<terminal-id>/<digest>.json`（atomic write）。shard 側は digest と revision だけを持つ |
| 書く契機 | 画面が変化しており、かつ **同一 terminal について最短間隔（既定 5s）を超えた**とき。加えて出力が静まった時点（quiescence）に 1 回 |
| 書く内容 | negotiated revision 2 の semantic screen checkpoint（可視 grid・bounded scrollback・cursor・saved cursor・scroll region・SGR・active buffer・decoder の途中状態）と観測時刻 |
| 書かない内容 | raw byte journal、environment 値、provider-native ID |
| 実行位置 | PTY output の critical section の**外**。専用の checkpoint worker が bounded queue から受け取る（[PR 検出の投影](../05-daemon.md#pr-検出の投影)と同じ形） |
| retention | checkpoint object の実 byte 数を [final retention と aggregate GC](../05-daemon.md#final-retention-と-aggregate-gc) の aggregate byte budget に計上し、同じ eviction 順序に載せる。安全に evict できない上限到達時は live terminal の次 checkpoint を skip し、既存 history を無断で落とさない |

document を shard から分けるのは、shard が whole-document の compare-and-swap だからである。数十 KiB の screen を
shard に埋めると、launch・exit・rollover のたびにその byte を書き直すことになり、CAS の衝突窓と write 量が
terminal の出力量に比例して増える。

書き込み順序と crash boundary は次のとおりで、**shard が名指す immutable object だけを採用する**。
同じ pathname への上書きでは C1〜C2 の間に旧 bytes が失われるため、digest ごとに別 object を作り、旧 object は
参照の切替後まで残す。

```text
C1  immutable screen object write   temp -> fsync -> rename（既存 object は上書きしない）
C2  shard CAS               checkpoint digest + revision を記録
C3  unreferenced object GC  shard / retained history の参照 0 を確認して削除
```

| crash 境界 | durable state | 次の起動が表示するもの |
|---|---|---|
| C1 より前 | 前回の checkpoint | 前回の画面（観測時刻付き） |
| C1〜C2 | 新旧 object、shard は旧 digest | **旧 checkpoint**。新 object は未参照として後で回収する |
| C2〜C3 | 新旧 object、shard は新 digest | 新しい画面。旧 object は未参照として後で回収する |
| C3 の後 | shard と新 object | 新しい画面 |

「新しそうな方を採る」ことをしないのが要点である。checkpoint は利用者が読む画面そのものなので、
継続性を証明できない bytes を画面として出すと、存在しなかった出力を見せることになる。参照先の欠落、digest
不一致、未知 schema はその history の画面だけを typed `unavailable` にし、別 object や live terminal へ fallback
しない。object は terminal screen を含み得るため data directory と同じ owner-only directory / file mode を必須とし、
path や内容を log へ出さない。

restart 後、reconcile が `identity_unknown` にした record にも checkpoint がある。inventory は interrupted entry に
filesystem path ではない opaque な `ScreenCheckpointRef` と観測時刻だけを載せる。TUI は exact な旧
`TerminalRef` とその ref を `terminal_history_snapshot` request へ返し、daemon が shard の参照と digest を再検証して、
既存の bounded semantic screen payload として返す。これにより inventory frame を checkpoint の個数に比例させず、
client に daemon data directory の path や直接 read authority を渡さない。TUI は得た payload を read-only の body として描く。
live tab とは別 identity（完全な旧 `TerminalRef`）で持ち、attach・input・resize・stream の `resume` を一切送らない。
label と body の両方に「この画面は `<時刻>` 時点の最後の観測であり live ではない」ことを出す。

### workspace restore plan

daemon が workspace scope の復帰候補を 1 つの projection として返し、利用者が 1 操作で実行できるようにする。

| 段 | 内容 |
|---|---|
| plan | `restore_plan` request が、その workspace の復帰候補を返す。各 item は kind（agent / terminal）、continuation、scope、`resumable`、safe reason だけを持つ。read-only であり、effect を持たない |
| 確認 | TUI は plan を modal として見せる。**自動実行しない**（既存の明示 resume 契約を維持する） |
| 実行 | item ごとに**独立した `OperationId`** で `ResumeAgent` / `ResumeTerminal` を送る。delivery outcome が確定するまで、TUI は source と operation の対応を durable intent に保持して同じ ID で照会・再送する。1 件の失敗は他 item を巻き込まない |
| 収束 | 同じ operation は idempotency ledger、別 operation になった再実行は durable source → replacement relation で既存 final へ収束する。実行中に TUI が落ちても、同じ source から二重 spawn しない |

- **枠を空けるために live を落とさない。** Agent concurrency と capacity pool の admission はそのまま適用し、
  枠が足りない item は typed `resource_exhausted` で個別に失敗する。plan は部分成功で終わってよく、残りは
  従来どおり tab ごとの操作で再試行できる。
- plan に載るのは、daemon が exact target を検証できた候補だけである。metadata 欠落・scope 不一致・supersede 済みの
  lineage は `resumable: false` として理由付きで列挙し、実行対象にしない。
- plan の並びは continuation と exact ref の決定的順序であり、TUI の保存済み表示順
  （[pane 復元](../03-tui.md#workspace-open-時の-pane-復元)）がある lineage はその位置を保つ。

## 復帰しないもの

「戻らないもの」を明示することが、この機構の安全性の半分である。次はいずれも **設計上戻さない**。

| 対象 | 理由 |
|---|---|
| PTY master fd と child process | crash した process の fd は復元不能。PID だけでは所有権を証明できない（[generation と orphan safety](../05-daemon.md#generation-と-orphan-safety)） |
| shell の中で走っていたコマンド | daemon はそれを所有しておらず、再実行は破壊的になり得る |
| shell の履歴・環境の途中状態 | generic terminal の durable record が持つのは trusted profile・scope・geometry であり、shell 内部状態は capture しない。resume 時の program・cwd・environment は current profile / scope から再解決する |
| MCP caller credential | `daemon_minted_ephemeral` であり、restart は明示的な失効境界である |
| agent が hook で報告した phase | in-memory の refinement であり、restart 後は観測 state 由来の phase に戻る |
| ACK を失った in-flight input | 二重実行になる。既存の outcome 照会と fence をそのまま使う |
| 中断された create / initialize の effect | 既存どおり safe failure として明示 recovery を待つ |
| metrics counter と subscriber | process-local な観測であり、0 から始まる |

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| crash 後に自動で resume する | agent / shell が途中の command を再実行し得る。既存の「明示 resume だけが発火する」契約が守っているのはこの危険であり、restart の便利さのために外さない |
| raw output journal をそのまま durable にして replay する | 任意 byte 境界で切れた tail は UTF-8 / CSI / OSC の途中から始まり、retention も byte 数に比例して膨らむ。semantic checkpoint を採る判断は [12. terminal VT snapshot](12-terminal-vt-snapshot.md) と同じ理由で一貫している |
| screen checkpoint を shard の record に埋める | shard は whole-document CAS なので、出力量に比例して write と衝突窓が増える |
| generic terminal を「同じ cwd で再起動」する形にし、lineage を持たせない | client か record の path を信じることになり、[terminal launch environment](../05-daemon.md#terminal-launch-environment) の「client は path・argv・environment を指定できない」契約を崩す。lineage を持たせれば daemon 側で scope を解決し直せる |
| [PTY broker](07-pty-crash-continuation.md) を先に実装する | broker の着手条件（crash 継続が製品要件になる・常駐コストの計測・supervisor と upgrade 運用の承認）を満たしていない。本書の機構は broker 無しで取れる価値だけを取る |

## 段階と issue 分割

各段は依存する前段までを含めて独立に出荷でき、途中の段階でも既存の live restore / Agent resume 契約を壊さない。

| 順序 | issue | 成果物 |
|---|---|---|
| 1 | `feat(core): generic terminal continuation ref と resume target` | typed ID、`TerminalResumeTarget`、reducer の relation（`resumed_from` / `superseded_by`）、fence の pure test |
| 2 | `feat(daemon): ResumeTerminal と trusted profile の再解決` | wire request、profile / scope の再検証、L1..L5 の書き込み順序、idempotency replay |
| 3 | `feat(daemon): durable screen checkpoint` | immutable object、throttle と quiescence、digest fence、参照付き GC、retention budget への統合 |
| 4 | `feat(ipc): interrupted entry から checkpoint を取得可能にする` | opaque checkpoint ref と `terminal_history_snapshot`、capability negotiation、bound 違反の fail closed |
| 5 | `feat(tui): interrupted generic terminal tab と read-only last screen` | interrupted 投影の generic 対応、live でないことの label / body、`Ctrl-O r` の resume |
| 6 | `feat(daemon+tui): workspace restore plan と 1 操作 restore` | `restore_plan` projection、確認 modal、item ごとの独立 operation、部分成功の表示 |
| 7 | `test(root): SIGKILL → restart → restore の実 PTY E2E` | 実 daemon を `SIGKILL` し、restart 後に plan・checkpoint・resume が同じ結果へ収束することを shipping binary で確認する |

段 3 は段 1・2 が無くても Agent の interrupted tab に対して単独で価値を出せる。段 4 は段 3、generic terminal を
投影する段 5 は段 1・2・4 に依存する。段 6 は段 1・2 が入って初めて generic terminal を含む plan になり、
段 7 は 1〜6 を shipping binary で結合する。

## PTY broker との関係

[7. PTY crash 継続](07-pty-crash-continuation.md) は「crash 後も同じ PTY へ attach できる」を目指す。本書は
「crash 後に同じ作業面へ戻れる」を目指す。両者は排他ではなく、broker を後から導入しても本書の成果は無駄にならない。

| 資源 | 本書 | broker 導入後 |
|---|---|---|
| lineage identity（Agent / terminal） | 本書が導入する | そのまま使う。broker terminal も同じ lineage に属する |
| 最後の画面 | durable checkpoint（read-only） | broker が生存していれば live snapshot、していなければ本書の checkpoint へ fallback |
| 復帰の起動点 | `restore_plan` + 明示操作 | 同じ plan が、live な broker terminal を「再 attach」候補として含む |

したがって、本書は broker の前提条件を作りこそすれ、broker の設計判断を先取りしない。broker を採らない場合でも
G1〜G3 は解消される。
