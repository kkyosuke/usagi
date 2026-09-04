# 19. Director screen graph

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [TUI の指示モード](../03-tui.md#指示モードdirector-mode) ｜ 関連提案: [goal-driven Work Run](18-goal-driven-work-run.md)

> **Status:** 採用・実装済み（2026-09-04）
>
> **Baseline:** commit `c1f7629ee19f698338826f12edb30d4be266e2a6`（2026-09-03）。現在の Director、Work Run
> 一覧・操作、Goal Composer の仕様は [TUI の指示モード](../03-tui.md#指示モードdirector-mode)を参照する。

Director 内の Organization、Work Run、Agent launcher、Agent conversation をどの画面へ置き、どの identity を保って
遷移するかを決めた設計記録である。表示素材の優先順で切り替えていた旧 drawer を 3 つの明示的な sub-route に整理した。
現在の実装仕様は [TUI](../03-tui.md#指示モードdirector-mode) が正本である。goal-driven と classic は route shell を共有するが、
Overview の scope と操作まで同一にはしない。

## 目次

- [決定](#決定)
- [情報階層](#情報階層)
- [3 画面の責務](#3-画面の責務)
- [画面遷移](#画面遷移)
- [Director Overview](#director-overview)
- [workflow ごとの成立条件](#workflow-ごとの成立条件)
- [Agent Launcher](#agent-launcher)
- [Agent](#agent)
- [入力と戻る操作](#入力と戻る操作)
- [状態と identity](#状態と-identity)
- [実装結果](#実装結果)
- [非同期更新と失敗時の着地](#非同期更新と失敗時の着地)
- [実装順序](#実装順序)
- [受け入れ条件](#受け入れ条件)
- [採用しない案](#採用しない案)

## 決定

3 画面は同格の tab ではなく、次の親子関係にする。

```text
Director Overview              常設のハブ
├─ Agent                       選択した既存 Agent の詳細
└─ Agent Launcher              新しい Agent / Work Run を作る一時フロー
   └─ confirm ───────────────► Agent（starting を含む）
```

- Director を開いた最初の着地点は Overview とする。
- goal-driven の Overview は Work Run の選択と、選択中 Work Run の Organization tree を同じ画面に置く。
- classic の Overview は root conversation の選択と、workspace 全体の Organization tree を同じ画面に置く。選択
  conversation で tree を絞り込まない。
- 既存 Agent を選ぶ操作は goal-driven では Overview の tree、classic では conversation rail が担う。Agent Launcher は
  新規作成専用で、既存 Agent の切替には使わない。
- Agent は 1 つの conversation / terminal に集中する詳細画面とする。Organization tree を常時併記しない。
- Agent Launcher は独立した永続対象ではなく、呼び出し元を持つ一時 route とする。cancel は呼び出し元へ、confirm は
  作成した Agent へ進む。
- drawer 全体を閉じる操作と、Agent から Overview へ戻る操作を分ける。

この構造なら「全体を見る」「新しく始める」「1 人と話す」の入力 owner が一意になり、terminal の `Esc` と navigation が
競合しない。

## 情報階層

画面名に `Goal Session` は使わない。usagi の `Session` は隔離 worktree を意味するため、目的の単位は UI では
`Work Run`、短い行見出しでは `Goal` と呼ぶ。

```text
Workspace
└─ Work Run / Goal                 目的、進捗、停止理由
   ├─ root Director Agent          目的を受け持つ conversation
   └─ Task
      └─ Session                   隔離 worktree と role
         └─ Agent                  実行 process / conversation
```

Work Run と Session を一対一にしない。1 つの目的が複数 Session へ委譲されることと、Session が worktree lifecycle を
持つことを UI でも区別する。Overview は複数の authority を stable identity で join するだけで、Work Run に
Session / Agent の lifecycle を複製しない。権威の詳細は
[goal-driven Work Run の情報階層](18-goal-driven-work-run.md#情報階層と権威)を正本とする。

classic には Work Run が存在しない。root Agent conversation と、`parent_session_id` で構成する Session tree は workspace を
共有するが、現在の authority には「どの root conversation がどの root Session を作ったか」を永続的に結ぶ identity がない。
そのため classic の conversation 選択を Organization の scope と解釈したり、表示順から関連を推測したりしない。

## 3 画面の責務

| 画面 | 答える問い | 主な表示 | 主な操作 |
|---|---|---|---|
| Director Overview | どの目的または conversation があり、workspace で誰が動いているか | workflow に応じた対象一覧、Organization tree、状態の要約 | 対象 / node 選択、tree 展開、Agent / Session への drill-down、新規開始 |
| Agent Launcher | 誰に新しい仕事を任せるか | launch scope、Goal、provider / profile、effective role / policy | 候補選択、Goal 編集、confirm、cancel |
| Agent | この Agent は何をしており、何を伝えるか | breadcrumb、状態、terminal / conversation、safe feedback | Agent 入力、scroll / copy、Overview へ戻る、新規開始 |

Overview は観測面、Launcher は作成面、Agent は対話面である。Overview から daemon mutation を起こすのは、明示的な
`New` と goal-driven に既存の typed Work Run action だけに限る。classic に架空の Work Run action を設けない。

## 画面遷移

```text
Workspace Home
    │ Director toggle
    ▼
Director Overview ── Enter on root Agent ───────► Agent
    │  │                                           │
    │  ├─ Enter on managed Session / worker ─────► existing Session Closeup
    │  │                                           │
    │  └─ New ─────────────► Agent Launcher ◄─────┘ New
    │                           │       │
    │                         cancel  confirm
    │                           │       │
    ◄───────────────────────────┘       └────────► Agent (starting → live)
    │
    └─ Director toggle ──────────────────────────► Workspace Home
```

managed Session / worker の terminal を Director 内へ複製しない。tree からそれらを選んだ場合は既存 Session Closeup を
開き、戻ると同じ `WorkRunId` と tree node selection を持つ Overview へ戻す。root Director Agent だけが Director の
Agent 画面に載る。この境界は [goal-driven Work Run の既存画面との境界](18-goal-driven-work-run.md#既存画面との境界)と
一致する。

Director toggle で drawer を閉じた場合は Director の sub-route を保持し、再度開いたときは直前の画面を復元する。
ただし初回、選択 identity の消失、復元不能な schema の場合は Overview へ着地する。drawer の開閉は背面の Home route、
active Session、pane selection を変更しない。

## Director Overview

通常幅では Goal rail と Organization tree の 2 領域を同時に表示する。Goal rail の選択が tree の scope を決める。

```text
┌─ ♛ Director / Overview ─────────────────────────────────────┐
│ Goals                         Organization                  │
│ > OAuth ログインを追加         ♛ Director        running    │
│   Working · 4/6               ├─ ◆ planner       waiting    │
│   検索を高速化する             ├─ ◆ auth-api      running    │
│   Waiting for you             │  └─ ● reviewer   ready      │
│   README を再構成する          └─ ◆ auth-ui       stopped    │
│   PR #1841 ready                                             │
│                                                              │
│ + New goal             Enter: action/open  Space: expand    │
└──────────────────────────────────────────────────────────────┘
```

Goal の並びは `Waiting for you`、`Stopped`、実行中、完了時刻の新しい順とする。選択変更だけでは Agent を開かない。
tree は選択中 Goal の provenance に属する node だけを表示する。手動起動した root conversation や goal に属さない Session は
Goal の一部にせず、rail の固定 synthetic scope `Unassigned` から workspace-wide tree として開く。全 Goal の node を 1 本の
tree へ混ぜない。

Goal rail と Organization tree は別の focus region とする。Goal rail の `Enter` は現在の Work Run 操作面と同じく、その状態で
許可された typed action を開く。cancel や escalation 解決の確認は Overview 内の一時 substate であり、第4の常設画面には
しない。tree の `Enter` は選択 node の drill-down だけを行う。

tree node の種類は icon と label の両方で区別する。状態は色だけでなく短い状態語を併記する。Goal が 0 件でも
Organization を空にせず、`No goals yet`、`New goal`、`Unassigned` を表示する。

狭幅では 2 領域を上下へ積み、まず Goal、次に選択 Goal の tree を表示する。別画面へ分割しないため、terminal resize で
navigation depth が変わらない。

## workflow ごとの成立条件

3 route と戻る規則は両 workflow で共有する。一方、対象の authority、Overview の scope、許可する action は次のように
分ける。

| 観点 | goal-driven | classic |
|---|---|---|
| Overview の左側 | Work Runs / Goals | root Agent conversations |
| 左側の選択が決めるもの | 選択 Work Run の Organization scope | 開く Agent。Organization scope は変えない |
| Organization | Work Run provenance で絞った tree。`Unassigned` だけ workspace-wide | 常に workspace-wide の Session tree |
| 既存 root Agent を開く | 選択 Work Run に属する root Agent node | conversation rail の項目 |
| 新規作成 | Goal + provider から Work Run を開始 | provider から conversation を開始 |
| typed action | cancel / escalation 解決など Work Run action | なし |
| Agent の next / previous | 同じ Work Run に属する retained root Agents | retained root conversations 全体 |

goal-driven で Work Run と root Director Agent を結ぶ join は、`SupervisorRunQuery.provenance` の
`worker_session_id: None` と `worker_agent_id` を使う。TUI は terminal の順番、goal 文字列、時刻から関連を推測しない。
inventory に同じ stable Agent identity の retained root tab がある場合だけ Agent への drill-down を有効にし、消失時は
Overview に留まって safe feedback を表示する。

classic では conversation と Organization が同じ workspace に並ぶだけで、親子関係ではない。conversation の `Enter` は
Director の Agent へ、Organization の Session / worker の `Enter` は既存 Session Closeup へ進む。この非対称性を保てば、
同じ route shell のまま authority を偽らずに両 workflow が成立する。

## Agent Launcher

Launcher は `LaunchContext` を表示し、何を作るのかを confirm 前に明示する。

| workflow / 呼び出し | launch scope | 必須入力 |
|---|---|---|
| goal-driven の `New goal` | workspace root に新しい Work Run | Goal、provider / profile |
| classic の `New conversation` | workspace root に root Agent conversation | provider / profile |
| 将来の明示的な task delegation | 選択 Work Run / task | role、provider / profile、task instruction |

```text
┌─ Start Work Run ─────────────────────────────────────────────┐
│ Goal                                                         │
│ OAuth ログインを追加し、PR を review-ready にする            │
│                                                              │
│ Agent                                                        │
│   claude                                                     │
│ > codex                                      workspace default│
│   sakana.ai                                                  │
│                                                              │
│ Role  Director    Delivery  PR ready    Policy  Standard     │
│ Esc: cancel                                      Enter: start │
└──────────────────────────────────────────────────────────────┘
```

Goal と provider を別々の wizard step にしない。submit される値を 1 画面で確認でき、通常フローの確認回数を増やさないためで
ある。候補選択は install 済み provider の closed vocabulary とし、設定済み default を強調するが自動 submit しない。

Launcher は `return_to` を保持する。Overview の `New goal` / `New conversation` から開いて cancel した場合は同じ Goal /
tree selection の Overview、Agent の同名 action から開いて cancel した場合は同じ Agent と scroll state へ戻る。confirm 後は
呼び出し元へ戻らず、新しい Agent 画面へ進む。

## Agent

Agent は conversation / terminal の入力を最大化する。Goal と Organization の詳細を同居させず、breadcrumb と短い状態だけを
残す。

```text
┌─ ♛ Director / OAuth ログインを追加 / Director ── running ───┐
│ [ Overview ]  [ New goal ]                    Agent 1 of 2    │
│                                                              │
│  terminal / conversation                                    │
│                                                              │
│                                                              │
│ Ctrl-O w: overview  Ctrl-O [/]: previous/next                │
│ Ctrl-O g: close                                               │
└──────────────────────────────────────────────────────────────┘
```

`next / previous` は同じ Goal に retained root conversation が複数ある場合だけ、その stable order 内を巡回する。別 Goal へ
暗黙に移らない。別 Goal の Agent へ移る場合は Overview を経由し、現在の context が breadcrumb から突然変わらないように
する。classic workflow では Overview の `Conversations` に属する root conversation 全体が同じ巡回 group になる。

goal-driven の action label は `New goal`、classic は `New conversation` とし、汎用的な `New` だけを表示しない。現在の
Agent の子を作る操作に見せず、新しい sibling を作ることを明示するためである。

Agent が interrupted / stopped になっても Agent 画面を Overview へ自動遷移させない。terminal の代わりに safe reason と
`Resume` / `Retry` / `Overview` のうち daemon projection が許可する action を表示する。履歴の保持や exact resume は現在の
[interrupted Agent](../03-tui.md#interrupted-agent-の-tab-投影と明示-resume)契約を再利用する。

## 入力と戻る操作

| context | `Enter` | `Esc` | `Ctrl-O w` | `Ctrl-O g` |
|---|---|---|---|---|
| Overview | 選択 node を開く | Director を閉じる | no-op | Director を閉じる |
| Agent Launcher | confirm | cancel して `return_to` へ戻る | cancel して `return_to` へ戻る | Director を閉じ、Launcher draft を保持する |
| Agent（live） | Agent PTY へ送る | Agent PTY へ送る | Overview へ戻る | Director を閉じる |
| Agent（non-live） | 選択中の明示 action | Overview へ戻る | Overview へ戻る | Director を閉じる |

`Esc` を live Agent から戻る操作にしない。Agent CLI が中断・取消に使う既存 contract を守るためである。Overview へ戻る
操作は既存 Work Run 操作面の `Ctrl-O w` と visible button に一本化する。`Ctrl-O g` は drawer 全体の toggle のまま保ち、
Overview back と Director close を別 intent にする。Agent の previous / next は既存 tab 操作の `Ctrl-O [` / `Ctrl-O ]` を
再利用する。

mouse は描画と同じ hitbox が stable identity を返す。Goal と tree node の single click は stable identity の選択、
`Enter` は drill-down、breadcrumb / button は single click とする。terminal 領域の pointer は Agent が所有し、背景の
Overview へ fallthrough させない。

## 状態と identity

画面は `terminal_view` や Organization row の有無から推測せず、明示的な route state で表す。

```text
DirectorRoute
├─ Overview
│  ├─ GoalDriven { selected_work_run_id | unassigned, selected_node_id? }
│  └─ Classic { selected_conversation_id?, selected_node_id? }
├─ Launcher { return_to, launch_context, draft, operation_id? }
└─ Agent { context: work_run_id | classic, agent_runtime_id | pending_operation_id }
```

`return_to` は route 全体の任意 stack ではなく、`Overview` または `Agent` のどちらか一段だけに制限する。Launcher から
Launcher を開かず、navigation stack の無制限な増加を防ぐ。

選択 identity と表示 label / 配列 index を分ける。Work Run、Session、Agent runtime、pending launch はそれぞれ daemon の
stable ID または producer の `OperationId` で fence する。描画順が変わっても選択対象を変えない。

## 実装結果

baseline で表示素材の優先順から暗黙に決めていた画面を `DirectorRoute` に置き換え、Overview / Launcher / Agent の
foreground input owner と戻り先を reducer で一意にした。Overview は Work Run、Session、root Agent inventory を stable ID で
join し、goal-driven では provenance scope、classic では workspace scope を使用する。描画と hit-test は同じ projection を読み、
選択直後の terminal / Closeup projection まで同じ identity を渡す。

既存の Work Run typed action、exact operation retry、interrupted Agent resume、root Agent tab cycle は authority を変えず再利用した。
production PTY test は Overview からの drill-down、Launcher の cancel / confirm、drawer close / reopen、empty / pending / interrupted、
goal-driven の cold restart を固定している。

## 非同期更新と失敗時の着地

- confirm すると、同じ `OperationId` を持つ pending Agent を tree に加え、Agent の `starting` 画面へ直ちに進む。
- daemon の final が exact scope / operation / semantic digest と一致した場合だけ pending identity を live Agent identity へ
  置換する。応答喪失後の replay で Agent や Work Run を増やさない。
- launch failure で pending identity が失われた場合は safe reason を Overview に残して戻る。別 Agent へ自動選択せず、
  retry は新しい明示操作として行う。
- 表示中 Agent が inventory から消えても、retained history があれば stopped / interrupted として同じ Agent 画面に留める。
  history も無ければ、別 Agent へ自動選択せず Overview へ戻る。
- 選択 Work Run が削除された Overview は、表示順上の surviving Work Run へ決定的に着地する。無ければ中立な empty state と
  し、`New goal` を暗黙に選択しない。
- reconnect / resync 中も `DirectorRoute` を保ち、projection を `State unavailable` にする。route と target の消失を同一視
  しない。

## 実装順序

1. 現在の drawer に visible conversation selector と stable-ID hitbox を追加し、通常面の `Ctrl-O [` / `Ctrl-O ]`、click、選択後の
   terminal projection を production input route まで通す regression test で固定する。
2. `DirectorRoute` と各 route の stable selection を reducer に追加し、現在の projection precedence を明示 route へ置き換える。
3. workflow ごとの scope 契約に従って Work Run、Organization、root Agent inventory を join した Overview renderer、keyboard /
   mouse navigation を追加する。goal-driven の root Agent drill-down は daemon projection に明示 identity が追加された後に有効にする。
4. 現在の picker / Goal Composer を `Launcher` へ移し、`return_to` と confirm 後の pending Agent 遷移を追加する。
5. root Agent terminal を `Agent` route へ移し、Overview back、workflow ごとの next / previous、non-live action を追加する。
6. Goal rail を現在の `SupervisorRunId` keyed projection に接続し、task provenance から Session Closeup への drill-down を
   追加する。既存の typed cancel / escalation action と exact operation retry は Overview 内の substate として移す。
7. production screen graph test で 3 route、drawer close / reopen、resize、reconnect、pending / failure / interrupted を固定する。

現在の Work Run projection が持つ並び順、freshness、typed action、exact operation retry をそのまま再利用する。Overview への
統合は authority の変更ではなく navigation の再編であり、terminal output から task progress や成功を推測しない。

## 受け入れ条件

- Director 初回 open は Overview へ着地し、既存 Agent があっても自動的に terminal 入力へ focus しない。
- Overview の Goal 選択と tree node 選択は stable identity で保持され、refresh / sort / resize で別対象へ移らない。
- Overview、Launcher、Agent のどれが前面 input owner か 1 frame ごとに一意である。
- Launcher の cancel は exact `return_to` へ戻り、confirm は 1 request / 1 pending Agent / 1 live Agent へ収束する。
- live Agent の `Esc` / 通常文字 / paste は navigation に奪われず、PTY へ 1 回だけ届く。
- Overview back と Director close は異なる intent で、どちらも背面 Home の target / pane state を変更しない。
- root Agent は Director の Agent 画面、managed worker は既存 Session Closeup に開き、同じ terminal を 2 画面へ重複投影しない。
- 通常の Agent 画面では visible selector の click と `Ctrl-O [` / `Ctrl-O ]` が同じ stable identity を選び、次の frame で
  terminal / conversation 表示がその対象へ移る。一時面が切替を所有しない場合は、その理由と戻る操作を footer に表示する。
- launch failure、interrupted、projection unavailable で別 Agent へ silent fallback しない。
- goal-driven と classic の両方が同じ 3 route と戻る規則を使う。Overview の scope、typed action、Agent 巡回 group は
  [workflow ごとの成立条件](#workflow-ごとの成立条件)どおり分離される。

## 採用しない案

- **3 画面を同格 tab にする**: Launcher が常駐 navigation になり、「見る」と「作る」が同じ重さになる。cancel の戻り先も
  曖昧になる。
- **Organization tree を Agent 画面へ常時併記する**: terminal 幅を削り、tree refresh と PTY pointer の input owner が競合する。
- **表示データの有無で画面を決める**: inventory の遅延や reconnect のたびに Organization と Agent が切り替わり、利用者の
  navigation intent を失う。
- **Goal と Session を同じ階層に並べる**: 目的と worktree の一対多関係が見えず、Work Run の完了と Session の終了を混同する。
- **Agent Launcher を既存 Agent selector と兼用する**: 新規作成と既存対象への移動で `Enter` の副作用が変わる。既存 Agent の
  選択は goal-driven の Overview tree または classic の conversation rail に限定する。
- **live Agent の `Esc` で Overview へ戻る**: Agent CLI の中断操作を TUI navigation が奪う。
