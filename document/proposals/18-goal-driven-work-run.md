# 18. goal-driven Work Run

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [TUI](../03-tui.md) / [daemon](../05-daemon.md) / [MCP](../07-mcp.md) / [session role](../10-session-roles.md)

> **Status:** 一部採用済み・一部提案中の設計履歴
>
> **Baseline:** 原版 commit `f229360d5fd41c2affaa496ee8993c1026454b13`（2026-09-01）。goal-bound Director v1 の現在仕様は [TUI](../03-tui.md#goal-driven-workflow) と [daemon](../05-daemon.md#agent-admission-transaction) を参照する。SupervisorRun dashboard / verifier など本文で後続とした段階は現在契約ではない。

「目的を一度入力すれば、PR が Ready for review になるか、明示的な人間判断が必要になるまで、再プロンプトなしで
進む」goal-driven UI の target design である。Config、Goal Composer、goal-bound Director admission からなる v1 は実装済みで、
現在仕様は [TUI](../03-tui.md#goal-driven-workflow) と [daemon](../05-daemon.md#agent-admission-transaction) を正本とする。
durable SupervisorRun の compact な Active work banner、Director 内 task progress、Goal submit から workspace 所有 Run への
idempotent な昇格、root Agent dispatchとのdurableな相関とterminal進捗を現在の画面とdaemonが提供する。本書の複数 run 一覧、選択可能な独立 Run Closeup、Run-scoped team defaults、
独立PR/CI verificationは後続提案である。

現在の usagi は session、Agent、terminal、Director、Garden、durable decision、supervisor aggregate を個別に持つ。
本提案はそれらを置き換えず、利用者が投入した 1 つの目的を **Work Run** として束ねる。画面の主語を
「どの session を操作するか」から「目的がどこまで進み、次に誰の行動が必要か」へ変える。

## 目次

- [目標と非目標](#目標と非目標)
- [現在の v1](#現在の-v1)
- [情報階層と権威](#情報階層と権威)
- [画面フロー](#画面フロー)
- [各画面](#各画面)
- [状態と停止理由](#状態と停止理由)
- [既存画面との境界](#既存画面との境界)
- [daemon と TUI の接続](#daemon-と-tui-の接続)
- [再接続と複数 Run](#再接続と複数-run)
- [安全性と既定 policy](#安全性と既定-policy)
- [段階的な実装](#段階的な実装)
- [受け入れ条件](#受け入れ条件)
- [採用しない案](#採用しない案)

## 目標と非目標

### 目標

- 通常操作では目的だけを入力し、team、base branch、実行 policy、Agent profile は見える既定値を使って開始できる。
- 計画、委譲、実装、検証、PR 作成、CI 確認を 1 つの Work Run として追跡する。
- 人間の回答なしに安全に進めない場合だけ `Waiting for you` に止まり、質問、影響範囲、選択肢を表示する。
- 実行不能・上限到達・verification failure では、単なる `failed` ではなく停止理由と許可された回復操作を表示する。
- TUI や daemon を再起動しても同じ Run を復元し、受理済み task や worker を二重実行しない。
- session、Agent、PR、decision の既存 identity と authority を再利用し、Work Run 用の第二の lifecycle を作らない。

### 非目標

- PR の自動 merge。既定の完了点は必須 CI が成功した `Ready for review` とする。
- Agent の判断内容が正しいことの保証。daemon は policy、fence、順序、verification を保証する。
- terminal 出力や provider 固有会話を Work Run の state として複製すること。
- すべての作業を Work Run に強制すること。単独 session、terminal、手動 Director は残す。
- 未実装の supervisor loop を UI だけで動いているように見せること。

## 現在の v1

v1 は既存の Director、session delegation、user decision、PR inventory を一つの goal-bound root Agent から利用できるように
する縦切りである。Work Run の新しい lifecycle や成功判定は追加せず、既存 authority のまま再 prompt を減らす。

```text
Config: Workflow = goal-driven（既定は classic）
  -> Director / New
  -> Goal Composer（Goal + installed provider）
  -> daemon AgentGoal admission
  -> fixed autonomous operating contract + user Goal
  -> root Director
       ├─ existing session / delegation / worker
       ├─ existing user decision
       └─ existing PR inventory / modal
```

- `work_mode` は Global の workspace 初期値と Workspace override に保存する。欠落・未知値は `classic` であり、既存操作を
  自動的に変更しない。
- Goal は非空、最大 16 KiB で、TUI と daemon の両方が検証する。provider は machine に install 済みの closed vocabulary
  から明示選択する。
- `AgentGoal` は通常の `Agent` と別 request で、Goal を含む semantic key により replay と別目的の再利用を区別する。
- daemon は Goal と「Draft PR + required checks + review ready、または blocking user decision まで継続し、merge しない」
  operating contract を `initial_prompt` に入れて既存 admission transaction で起動する。
- 状態は Director terminal、Organization、Session/Garden、Decision、PR の既存面に表示する。v1 は会話出力を
  authoritative Work Run state と呼ばず、CI/PR 完了を独自に判定しない。

したがって v1 は目的を一度で渡して既存組織を走らせる入口を実装するが、daemon crash 後の objective-level resume、
typed task stop reason、PR/CI の独立した terminal condition はまだ保証しない。それらを満たす target が以下の
SupervisorRun-based design である。

## 情報階層と権威

Work Run は durable `SupervisorRun` の利用者向け名称と投影であり、別の永続 aggregate ではない。初期実装では
**1 Work Run = 1 SupervisorRun = 1 目的**とする。

```text
Workspace
└─ Work Run                     目的、進捗、停止理由、成果物
   └─ Task DAG                  計画、依存、verification
      └─ Session                隔離 worktree と role
         └─ Agent / Terminal    実行 process
            └─ Commit / PR     成果物
```

| 対象 | 権威 | Work Run での扱い |
|---|---|---|
| Run / task / policy / escalation | daemon の supervisor aggregate | TUI は revision 付き safe projection を表示する |
| session / worktree / role / lineage | daemon の session lifecycle | task provenance から参照し、複製しない |
| Agent / dispatch / inbox | daemon の dispatch runtime と store | task の実行者と structured outcome を参照する |
| user decision | daemon の user-decision store | Work Run の待機理由から既存回答面へ接続する |
| PR / CI | daemon の PR inventory と独立 verifier | task の自己申告ではなく verification の証拠に使う |
| terminal / conversation | daemon-owned PTY | 詳細表示だけ。Run の成功判定には使わない |

Work Run の一覧に必要な表示名は bounded な `display_title` として持つ。省略時は目的の最初の非空行から
決定的に作り、TUI が LLM の要約完了を待たない。task instruction、terminal output、provider ID、credential は
一覧 projection に含めない。

## 画面フロー

```text
Workspace Home
  │
  ├─ + Start work
  ▼
Goal Composer ── Start ──► Run Closeup
                              │
                              ├─ Planning
                              ├─ Dispatching / Working
                              ├─ Verifying / Preparing PR
                              │
                              ├─► Waiting for you ── Answer ──┐
                              │                                │
                              ├─► Stopped ── Retry / Replan ───┤
                              │                                │
                              └─► Completed ── Open PR         │
                                                               └─► 同じ Run を再開
```

画面遷移は target identity を保つ。Run から task の session を開くと既存 Closeup へ移り、戻ると同じ
`SupervisorRunId` と task selection へ戻る。Goal Composer の submit は durable idempotency key を 1 つ発行し、
応答喪失後の再送で Run を増やさない。

## 各画面

### Workspace Home

Home の第一ブロックを `Active work` とし、その下に既存 session 一覧を置く。利用者は session 名や role を先に
決めず、目的から始められる。

```text
┌─ Active work ───────────────────────────────────────────────┐
│ ! OAuth ログインを追加       Waiting for you   4/6 tasks   │
│ ● 検索を高速化する           Working           2 agents    │
│ ✓ README を再構成する        PR #1841 ready                │
│                                                              │
│ + Start new work                                             │
└──────────────────────────────────────────────────────────────┘
┌─ Sessions ───────────────────────────────────────────────────┐
│ ◆ auth-manager                                               │
│   ├─ ● auth-api-worker                                       │
│   └─ ● auth-ui-worker                                        │
└──────────────────────────────────────────────────────────────┘
```

並び順は `Waiting for you`、`Stopped`、実行中、完了時刻の新しい順とする。色だけに依存せず、icon、状態語、task 数を
常に併記する。`Enter` は Run Closeup、`+ Start new work` は Goal Composer を開く。

### Goal Composer

必須の編集欄は `Goal` だけとする。Team、Delivery、Policy、Base、Agent は submit される値を常に表示し、暗黙の
設定にはしない。`Advanced` は同じ値の変更面であり、通常フローに確認 step を増やさない。

```text
┌─ Start work ─────────────────────────────────────────────────┐
│ Goal                                                         │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ OAuth ログインを追加し、PR を完成させる                  │ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ Team       Hierarchical                                      │
│ Delivery   PR ready for review                               │
│ Policy     Standard                                          │
│ Base       main                                              │
│ Agent      workspace default                                 │
│                                                              │
│ [ Start ]                                      [ Advanced ]   │
└──────────────────────────────────────────────────────────────┘
```

workspace の Team が `none` の場合は `Hierarchical (recommended)` を Run draft に選ぶ。`Start` は画面に見えている
選択を明示的に確定するが、workspace 設定自体は書き換えない。Run は effective role catalog の revision と必要な
team policy を immutable snapshot として持ち、その Run から作る session の role admission にだけ使う。これにより
最初の利用で別の Config 画面を要求せず、後の workspace 設定変更で実行中組織の権限が変わらない。

submit 後に計画確認 modal は挟まない。安全に決められない要求だけを、開始後の typed decision として返す。

### Run Closeup

Run Closeup は会話ではなく、daemon projection から作る進捗面である。上段に全体状態と resource、中央に task DAG、
下段に現在の action と停止理由を置く。

```text
┌─ OAuth ログインを追加 ─────────────────────── Running ───────┐
│ Progress  ███████░░░  4 / 6 tasks       Agents 2 / 4        │
│                                                              │
│ ✓ 要件と受入条件を作成                                      │
│ ✓ API 認証を実装                    auth-api                 │
│ ● UI を実装中                       auth-ui                  │
│ ◌ 結合テスト                         waiting on auth-ui       │
│ ◌ Draft PR を作成                    waiting                 │
│ ◌ CI と Ready for review を確認      waiting                 │
│                                                              │
│ Now: auth-ui-worker がフォームを実装中                       │
│ Stop reason: —                                               │
│                                                              │
│ [Garden] [Session] [Details] [Discuss] [Cancel]               │
└──────────────────────────────────────────────────────────────┘
```

task 行には `Pending / Ready / Dispatched / Running / Waiting / Verifying / Succeeded / Failed / Cancelled`
を人向けの短い語へ写し、依存待ちは対象 task を表示する。session がある task の `Enter` は既存 session Closeup を
開く。`Discuss` は対応する root Director conversation を開くが、会話の有無で Run の state を推測しない。

### Waiting for you

Agent が要求する判断と supervisor escalation は、既存 user-decision modal と同じ visual component を使う。
store と authority は混ぜず、header に source を表示する。

```text
┌─ Decision required · OAuth ログインを追加 ──────────────────┐
│ OAuth callback URL をどちらにしますか？                      │
│                                                              │
│ > /auth/callback                                             │
│   /api/oauth/callback                                        │
│   自由入力                                                   │
│                                                              │
│ Paused: auth-api                                             │
│ Continuing: auth-ui, documentation                           │
│                                                              │
│ [ Answer and resume ]                                        │
└──────────────────────────────────────────────────────────────┘
```

回答は同じ Run と task generation へ fence し、解決後に新しい Run を作らず再開する。独立 task は policy が許す範囲で
継続できる。Run 全体を止める escalation では `Continuing: none` と明示する。

### Stopped

`Failed`、`Escalated`、authority 不明、resource 上限到達を一つの曖昧な error banner に畳まない。Run Closeup の
`Stop reason` に typed reason と evidence を表示し、その reason が許可する action だけを出す。

```text
Status: Waiting for you

Stopped because:
integration-test が 2 回失敗し、retry 上限に達しました。

Evidence:
tests/oauth_callback.rs · expected 302, received 401

[ Retry ] [ Ask Director to replan ] [ Cancel ] [ Details ]
```

daemon を観測できない場合は `State unavailable · last observed 14:32` とし、Run 自体を `Failed` に変更しない。
stale cache と authoritative failure を描き分ける。

### Completed

既定 Delivery の完了条件は canonical PR が存在し、必須 CI が成功し、Draft が解除されて `Ready for review` に
なったことである。worker の summary や URL 出力だけでは完了にしない。

```text
┌─ Work completed ─────────────────────────────────────────────┐
│ ✓ Implementation complete                                   │
│ ✓ Tests passed                                               │
│ ✓ Required CI passed                                         │
│ ✓ PR ready for review                                        │
│                                                              │
│ PR #1842  feat: OAuth ログインを追加                         │
│ 12 files · +486 / -32                                        │
│                                                              │
│ [ Open PR ] [ View diff ] [ View report ]                    │
└──────────────────────────────────────────────────────────────┘
```

merge は Run 完了条件に含めない。将来 auto-merge policy を追加する場合も別の明示設定と verification を要求する。

## 状態と停止理由

TUI は新しい独自 state machine を持たず、supervisor state と task state から表示 stage を導出する。

| 表示 stage | authoritative state / evidence | 主 action |
|---|---|---|
| `Planning` | `Planning` または root task が判断中 | Details / Cancel |
| `Working` | `Running` かつ in-flight task あり | Session / Garden / Discuss |
| `Waiting for you` | pending decision または unresolved escalation | Answer / Cancel |
| `Verifying` | `Verifying` または PR/CI verifier 実行中 | PR / Details |
| `Completed` | `Succeeded` と required artifact verification | Open PR / Report |
| `Stopped` | `Failed` / `Escalated` / blocked next action | reason 別の回復操作 |
| `Cancelled` | `Cancelled` | Report |
| `State unavailable` | daemon projection を観測不能 | Reconnect / Details |

停止理由 projection は最低限 `reason_code`、safe `summary`、blocking task、観測時刻、safe evidence reference、
許可された action を同じ revision で返す。TUI は task が動かないことや terminal の無出力から理由を推測しない。

## 既存画面との境界

| 画面 | 継続する責務 | Work Run との関係 |
|---|---|---|
| Director | goal 入力、任意の相談、replan の判断主体 | Run state の書き手にはならない |
| Session sidebar / Closeup | worktree、Agent、terminal、diff、notes の詳細 | task provenance から drill-down する |
| Garden | session と Agent の全体観測 | 「誰が動いているか」を描き、Run progress は複製しない |
| Decision modal | 人間の回答入力 | source と Run/task fence を追加して再利用する |
| PR modal | repository の PR 一覧 | Completed の成果物から該当 PR を選択して開く |

Garden の区画を Work Run に変えない。1 Run は複数 session、1 session は将来複数 Run の保守作業に使われ得るため、
「1 区画 = 1 session、1 うさぎ = 1 Agent」という現在の意味を維持する。

## daemon と TUI の接続

```text
Goal Composer
  │ supervisor_start(idempotency key, goal, team/policy snapshot)
  ▼
daemon-owned SupervisorRuntime
  ├─ Ready task ── durable reservation ──► session dispatch
  ├─ dispatch outcome ───────────────────► reducer / next task
  ├─ decision or escalation ─────────────► TUI decision projection
  ├─ artifact contract ──────────────────► independent verifier
  └─ safe Work Run projection ───────────► Home / Run Closeup
```

最初に閉じるべき実装 gap は、Ready task を production の dispatch effect へ変換し、provenance を同じ durable
reservation に記録する scheduler 経路である。retry は `RetryReady` を再 dispatch へ接続し、artifact contract は
独立 verifier の `VerificationResult` が成功するまで task を成功にしない。

TUI は `SupervisorRunId` keyed の revision 付き projection を保持する。session/Agent/PR の表示情報は stable identity で
既存 projection と join し、Run snapshot にコピーしない。観測 lane は read-only で、画面を開いただけでは daemon、
session、Agent を起動しない。

## 再接続と複数 Run

- daemon は unfinished Run と effect reservation を起動時に reconcile してから新しい effect を admit する。
- TUI reopen は `Waiting for you` を最優先に、unfinished Run、最近の completed Run を bounded page で復元する。
- Goal Composer の応答を失っても同じ idempotency key で同じ Run を取得し、Home に重複行を作らない。
- task から session Closeup へ移動しても Run observation を止めない。戻ったときは同じ task selection を復元する。
- workspace ごとの同時 Run 数と run ごとの Agent 並列数は別の上限として表示する。
- 複数 Run が同じ base branch に成果を作る場合、統合順序や競合を推測せず typed escalation にする。

## 安全性と既定 policy

`Standard` policy は作成時に immutable snapshot として保存し、request ごとの隠れた上限緩和を許さない。少なくとも
dispatch 総数、Agent 並列数、task 深さ、retry 回数と backoff、decision timeout、required artifact contract を含む。

Goal Composer が表示する既定値は submit payload と一致させる。画面に `Team: Hierarchical` と表示しながら
workspace の `none` で実行する、または表示なしに workspace 設定を書き換える挙動は許さない。

Agent が次を自己申告しても、それだけでは terminal success にしない。

- test が成功した
- PR を作成した
- CI が通った
- review が完了した

成果物は commit/worktree fence、canonical PR inventory、required CI check など、artifact contract が選ぶ独立した
観測で検証する。検証不能は success ではなく `Waiting for you` または `Stopped` へ収束する。

## 段階的な実装

0. **goal-bound Director v1（実装済み）**: classic-default setting、Director 内 Goal Composer、goal を含む idempotent
   `AgentGoal` admission、固定 operating contract を接続する。既存 session / decision / PR surface を利用し、Run state は作らない。
1. **実行ループを閉じる**: Ready → dispatch → completion → next/retry → verification → terminal を daemon 内で接続し、restart を跨ぐ production E2E を追加する。
2. **read-only Run projection（一部実装済み）**: Home のcompact `Active work` bannerとDirector内task progressで既存 supervisor runを観測する。複数run一覧、選択可能な独立Run Closeup、cancel/drill-downは後続とする。
3. **Goal を SupervisorRun へ昇格する**: v1 の Goal submit と idempotent supervisor start は接続されている。
   target では visible defaults、Run-scoped team/policy snapshot、root Agent conversation の Discuss detail 化も加える。
4. **判断と停止理由**: user decision と escalation を共通 component へ投影し、typed reason と許可 action を接続する。
5. **PR completion**: canonical PR、CI、Draft/Ready を independent verifier と Completed 画面へ接続する。
6. **復元と polish**: reopen、複数 Run、narrow terminal、keyboard/mouse、screen graph、Garden/PR への遷移を固定する。

各段階は、未接続の action を表示しない。Goal Composer は既存daemon-owned root Agentを起動する面のままで、
それ自体をSupervisorRunとして表示しない。authoritative progressはdaemonに実在しworkspace ownershipを持つ
SupervisorRunだけを対象にする。

## 受け入れ条件

v1 の受け入れ条件は次のとおりである。

- settings 欠落、未知 token、既存 workspace は `classic` のままで、従来の New CLI picker と空 prompt launch が残る。
- `goal-driven` を明示した workspace だけが Goal Composer を開き、空 Goal を拒否して 1 回の submit から root Director を起動する。
- Goal は daemon の初期 prompt と operation semantic に含まれ、同じ operation の replay は 1 runtime、別 Goal は conflict になる。
- Goal launch も既存 Agent admission、root terminal correlation、session delegation、decision、PR observation を迂回しない。

target Work Run 全体の受け入れ条件は次のとおりである。

- 利用者が目的を 1 回 submit すると、追加 prompt なしで session/worker が作られ、fixture PR が Ready for review になる。
- 曖昧な要求は pending decision で止まり、回答後に同じ Run/task generation が再開する。
- daemon restart を planning、dispatch、completion、retry、verification の各境界へ注入しても worker と PR を重複作成しない。
- worker failure、NoReport、retry 上限、concurrency/depth/budget 超過、verification failure のすべてに safe な停止理由と回復 action がある。
- TUI が Run を観測できない状態と authoritative `Failed` を描き分ける。
- Run、task、session、Agent、PR を stable identity で辿れ、同名 session の再作成や別 PR を誤って採用しない。
- workspace Team が `none` でも別画面での事前設定を要求せず、画面に見えた Run-scoped team だけが使われる。
- 80×24 と最小対応端末で Goal Composer、Run、Decision、Stopped、Completed の全情報と主要 action に到達できる。
- screen graph test は keyboard/mouse hitbox、戻り先、decision 自動表示、stale revision、reconnect を固定する。
- 現在の単独 session、manual Director、Garden、terminal の操作契約を壊さない。

## 採用しない案

- **session 自体を Work Run と呼び替える**: 1 目的に複数 worktree が必要で、目的と実行場所の lifecycle が一致しない。
- **Director conversation を進捗の正本にする**: Agent 停止、context loss、再接続で task と成果物の確定状態を失う。
- **Goal submit 後に毎回 plan approval を要求する**: 通常フローに人間の再プロンプトを戻す。policy が判断不能な場合だけ decision にする。
- **Garden を Run dashboard に置き換える**: session/Agent の空間表現を失う。目的の進捗は Run Closeup、実行者は Garden と分ける。
- **worker の PR URL 出力で完了にする**: 誤った URL、別 session の PR、失敗 CI、Draft のままでも成功になり得る。
- **UI から先に作る**: state が進まないことを presentation で隠すため、daemon loop と production E2E を先に完成させる。
