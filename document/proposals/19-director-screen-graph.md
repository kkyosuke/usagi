# 19. Director screen graph

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [TUI の指示モード](../03-tui.md#指示モードdirector-mode) ｜ 関連提案: [goal-driven Work Run](18-goal-driven-work-run.md)

> **Status:** Adopted（2026-09-04）。本書は Director の画面名、階層、遷移、入力所有権の target design である。
> 現在実装済みの契約は [TUI](../03-tui.md#指示モードdirector-mode) と [キーバインド](../11-keybindings.md) を正本とする。
>
> **Baseline:** 原版 commit `f81ee1b2bc26e6649a21cae730cb713d20e7143d`（2026-09-03）。現在仕様は上記の TUI とキーバインドを参照する。

## 目的

Director の workspace-wide な組織観測、Work Run の選択、1 Run の進捗確認、root Director Agent との対話、新規開始を、
表示素材の優先順ではなく明示的な route と stable identity で表す。利用者が「全体」「仕事」「1 Run」「1 Agent」のどこに
いるかを breadcrumb と戻る操作から判断でき、terminal の入力を navigation が奪わないことを目的とする。

## 名称と情報階層

`Director` は機能全体と drawer shell の名前であり、配下の画面名には重ねない。`Overview` は選択した 1 Work Run の
観測画面だけに使い、PTY を持つ対話面は `Console` と呼ぶ。`Works` や `Goal Session` は使わず、daemon の
`SupervisorRun` に対応する利用者向け名称を `Work Run` に統一する。

```text
♛ Director
├─ Organization                  workspace 全体
├─ Work Runs                     Work Run の一覧
│  └─ Run Overview               選択 Run の進捗と構成
│     └─ Director Console        root Director Agent の PTY
└─ Start Work Run                新規作成
```

domain object の階層は次である。画面階層へ lifecycle を複製せず、stable identity で既存 authority を join する。

```text
Workspace
├─ Work Run / Goal
│  ├─ root Director Agent
│  └─ Task
│     └─ Session
│        └─ managed Agent
└─ unassigned root Director Agent / Session
```

Work Run と Session は一対一ではない。1 つの目的は複数 Session へ委譲でき、Session は隔離 worktree の lifecycle を持つ。

## 画面の責務

| 画面 | 主語 | 表示 | mutation |
|---|---|---|---|
| Organization | workspace | root Director、Session、managed Agent の workspace-wide tree | 新規開始だけ |
| Work Runs | Work Run の集合 | Goal、状態、進捗、短い ID | cancel、終了済み履歴の削除 |
| Run Overview | 選択 Work Run | Goal、進捗、task、停止理由、run-scoped Organization、成果物 | cancel、終了済み履歴の削除、既存回答面への遷移 |
| Director Console | 1 root Director Agent | breadcrumb、短い状態、terminal / conversation、safe feedback | Agent PTY 入力、close、resume |
| Start Work Run | 新規 Run draft | Goal、provider / profile、effective role / policy | Work Run 開始 |

### Organization

Organization は Director を初めて開いたときの着地点で、root conversation selector と workspace-wide tree を正式な画面へ
昇格したものである。goal に属さない対象もここから観測する。

```text
┌─ ♛ Director / Organization ────────────────────────────────┐
│ Directors                                                   │
│ > Codex · root                                  running     │
│   Claude · root                                 stopped     │
│                                                             │
│ Organization                                                │
│ ♛ Director                                      active      │
│ ├─ ◆ auth-api                                   running     │
│ │  └─ ● reviewer                                waiting     │
│ └─ ◆ auth-ui                                    ready       │
│                                                             │
│ Ctrl-O w: Work Runs  Ctrl-O n: Start  Esc: close           │
└─────────────────────────────────────────────────────────────┘
```

root Director の `Enter` はその Console、managed Session / Agent の `Enter` は既存 Session Closeup を開く。managed terminal を
Director 内へ複製しない。Session Closeup を開くときは Director を閉じるが、route と選択を保持し、`Ctrl-O g` で同じ位置へ
復帰する。

### Work Runs

Work Runs は Goal の集合を選ぶ画面である。`Enter` は mutation を起こさず、選択 Run の Run Overview を開く。action の
feedback は一覧の上へ挿入せず固定 footer に表示し、選択行の Y 座標を変えない。

```text
┌─ ♛ Director / Work Runs ───────────────────────────────────┐
│ > OAuth ログインを追加      Working          4/6           │
│   検索を高速化する          Waiting for you  2/5           │
│   README を再構成する       Completed        3/3           │
│                                                             │
│ ↑↓ select  Enter: Overview  Ctrl-C: cancel  Ctrl-X: delete │
└─────────────────────────────────────────────────────────────┘
```

並びは `Waiting for you`、停止中、実行中、完了時刻の新しい順とし、選択は `SupervisorRunId` で保持する。

### Run Overview

Run Overview は terminal 出力ではなく daemon の safe projection から 1 Run の状態を描く。task、Session、Agent、PR は
それぞれの authority を stable identity で join し、会話文から進捗や成功を推測しない。

```text
┌─ Director / Work Runs / OAuth ログイン / Overview ────────┐
│ Working                                      Run #01a06972 │
│ Goal  OAuth ログインを追加し review-ready にする           │
│ Progress  ███████░░░  4/6 tasks       Agents 2/4          │
│                                                             │
│ Tasks                                                       │
│ ✓ 設計を確定する                              done          │
│ ● API を実装する                              working       │
│ ! UI レビュー                                  waiting       │
│                                                             │
│ Organization                                                │
│ > ♛ Director                                   running       │
│   ├─ ◆ auth-api                                running       │
│   └─ ◆ auth-ui                                 waiting       │
│ Artifacts  PR #1841 · checks 5/6                            │
│ Esc: Work Runs  Enter: open  Ctrl-C: cancel                │
└─────────────────────────────────────────────────────────────┘
```

root Director の `Enter` は Console、Session / managed Agent は既存 Closeup、pending decision は既存 Decision、PR は既存
Pull Request 面へ進む。移動先から戻ったときは同じ `SupervisorRunId` と node selection を復元する。

### Director Console

Console は選択した root Director Agent の terminal を最大化する。Organization、task、progress、追加の command editor は
置かない。通常文字、IME 確定文字列、paste、`Enter`、`Esc`、`Ctrl-C` は managed Agent と同じ terminal input path で
PTY へ直接送る。

```text
┌─ Director / OAuth ログイン / Console ─────────── running ─┐
│ [ Overview ]                              Director 1 of 1  │
│                                                            │
│ terminal / conversation                                   │
│                                                            │
│                                                            │
│ Ctrl-O b: Overview  Ctrl-O w: Work Runs  Ctrl-O g: close  │
└────────────────────────────────────────────────────────────┘
```

Console の `Esc` と plain `Ctrl-C` を戻る・Run cancel に使わない。Agent CLI が持つ interrupt / dismiss contract を守る。
interrupted / stopped でも別画面へ自動 fallback せず、同じ Console に safe reason と `Resume` / `Overview` を表示する。

### Start Work Run

Start Work Run は Goal と provider / profile を同じ画面で確認する一時 route である。`return_to` は Organization、Work Runs、
Run Overview、Console のいずれか一段だけを保持する。cancel は exact `return_to`、confirm は pending Run の Run Overview へ
進む。

```text
┌─ ♛ Director / Start Work Run ──────────────────────────────┐
│ Goal                                                        │
│ OAuth ログインを追加し review-ready にする                 │
│                                                             │
│ Provider                                                    │
│   claude                                                    │
│ > codex                                     workspace default│
│   sakana.ai                                                  │
│                                                             │
│ Esc: cancel                                     Enter: start│
└─────────────────────────────────────────────────────────────┘
```

## 画面遷移

```text
Workspace Home
    │ Ctrl-O g / header
    ▼
Organization ── Ctrl-O w ──► Work Runs ── Enter ──► Run Overview
    │                            ▲   │                     │
    │ Enter root Director        │   │ Esc / Ctrl-O b     │ Enter root Director
    ▼                            │   └─────────────────────┘
Director Console ◄───────────────┼─────────────────────────┘
    │                            │
    └─ Ctrl-O b ──► Run Overview┘

Organization / Work Runs / Run Overview / Console
    └─ Ctrl-O n ──► Start Work Run
                       ├─ Esc / Ctrl-C ──► exact return_to
                       └─ matching success ─► Run Overview
```

`Ctrl-O b` は Director 内の一階層 back で、Console のように `Esc` を PTY が所有する面からも利用できる。Organization は
最上位なので `Ctrl-O b` は no-op、`Esc` または `Ctrl-O g` が drawer を閉じる。`Ctrl-O w` はどの Director route からも
Work Runs へ直接移動する。

drawer close は route、Run、node、Console の stable selection を保持する。再 open は直前の route を復元し、初回、対象消失、
復元不能 schema の場合だけ Organization へ着地する。背面の Home route、active Session、pane selection は変更しない。

## キー契約

| context | `Enter` | `Esc` | `Ctrl-O b` | `Ctrl-O w` | `Ctrl-C` | `Ctrl-X` |
|---|---|---|---|---|---|---|
| Organization | node を開く | Director close | no-op | Work Runs | no-op | no-op |
| Work Runs | Run Overview | Organization | Organization | no-op | active Run cancel 確認 | finished Run delete 確認 |
| Run Overview | node / artifact を開く | Work Runs | Work Runs | Work Runs | active Run cancel 確認 | finished Run delete 確認 |
| Director Console live | PTY | PTY | Run Overview / Organization | Work Runs | PTY SIGINT | PTY |
| Director Console non-live | 明示 action | parent overview | parent overview | Work Runs | no-op | no-op |
| Start Work Run | start | `return_to` | `return_to` | Work Runs | `return_to` | no-op |
| cancel / delete confirm | confirm | back | back | consumed | back | consumed |

`Ctrl-X` と `Ctrl-O x` は別操作である。Work Runs / Run Overview の plain `Ctrl-X` は終了済み Run の履歴削除、Console の
`Ctrl-O x` は選択 Agent conversation の close / dismiss を維持する。

Run cancel と delete は必ず確認を挟む。delete は `Succeeded / Failed / Cancelled` の terminal Run だけを対象にし、未完了
Run の `Ctrl-X` は mutation せず `Cancel the Work Run first` を固定 footer に表示する。finished Run の `Ctrl-C` も mutation
せず `This Work Run is already finished` を同じ位置に表示する。確認中の `Esc` / `Ctrl-C` は何も変更せず元画面へ戻る。

## workflow ごとの差分

goal-driven は上記の全 route を使う。classic には Work Run authority がないため Work Runs / Run Overview / Start Work Run を
捏造せず、Organization、Director Console、`New Conversation` のみを使う。classic の root conversation と Organization tree は
workspace を共有するが、表示順から親子関係を推測しない。

goal-driven の Work Run から Console へ進むには、daemon projection が redaction-safe な root Agent stable identity を公開する。
identity が無い Run では Director row を disabled にし、goal 文字列、時刻、terminal order から関連を推測しない。

## route state と identity

```text
DirectorRoute
├─ Organization { selected_conversation_id?, selected_node_id? }
├─ WorkRuns { selected_work_run_id? }
├─ RunOverview { work_run_id, selected_node_id? }
├─ Console { parent, agent_runtime_id | pending_operation_id }
└─ StartWorkRun { return_to, draft, operation_id? }
```

表示 label や配列 index は identity に使わない。Work Run、Session、Agent runtime、pending launch は daemon の stable ID または
producer の `OperationId` で fence する。refresh、sort、resize、reconnect で別対象へ選択を移さない。

## delete の権威

終了済み Work Run の履歴削除は daemon-owned supervisor store の mutation とする。TUI は `OperationId`、
`SupervisorRunId`、観測済み state revision を送り、daemon は workspace ownership、terminal state、revision を再検証してから
削除する。応答喪失後は同じ operation を replay し、重複削除を成功として同じ結果へ収束させる。削除後の snapshot から Run が
消えたことを確認するまで、TUI は別 Run の削除成功を推測しない。

## 非同期更新と失敗時の着地

- Start confirm は 1 request / 1 pending launch へ収束し、matching success の exact `SupervisorRunId` で Run Overview へ進む。
- launch failure は開始前の route を保持して safe reason を表示し、別 Run や Console へ silent fallback しない。
- cancel / delete の送信中は連打を消費する。結果不明は同じ `OperationId` の retry だけを許可する。
- 選択 Run が外部削除された Run Overview は tombstone を 1 frame 以上表示してから Work Runs へ戻せるようにする。
- reconnect / resync 中も route と stable selection を保ち、`State unavailable` と authoritative `Failed` を区別する。
- 一覧 feedback、確認、retry は固定 footer / fixed body slot を使い、Run row の Y 座標を変えない。

## 受け入れ条件

- 初回 Director open は Organization、再 open は直前 route と stable selection へ着地する。
- Work Runs の `Enter` は mutation を起こさず選択 Run の Run Overview を開く。
- Run Overview と Console は daemon projection / PTY の責務を混ぜない。
- Console に追加 command editor を置かず、通常入力を managed Agent と同じ path で PTY へ 1 回だけ送る。
- `Esc` は non-PTY route の back、Console の `Esc` は PTY input になる。
- `Ctrl-O b` は Console から parent overview へ戻り、`Ctrl-O g` はどの route でも drawer 全体を閉じる。
- plain `Ctrl-C` は Work Runs / Run Overview だけで Run cancel、Console では PTY SIGINT になる。
- plain `Ctrl-X` は finished Run の確認付き削除だけに使い、active Run や Console で誤削除しない。
- feedback 表示、refresh、sort、resizeで一覧行と選択 identityがずれない。
- managed Session / Agent は既存 Closeup、root Director Agent だけが Console に開く。
