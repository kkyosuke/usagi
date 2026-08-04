---
number: 643
title: feat(tui): Home sidebar に running / waiting / failed の session 件数を出す
status: in-progress
priority: medium
labels: [tui, ui, core]
dependson: []
related: []
created_at: 2026-08-04T12:55:39.153260+00:00
updated_at: 2026-08-04T12:55:46.679536+00:00
---

## 目的

Home の sidebar は session を 1 件ずつ 2 行で描くだけで、workspace 全体の状態を一目で掴む手段が無い。session が増えると「いま動いているのはどれか」「入力待ちで止まっているものがあるか」「作成に失敗した行が残っていないか」を、行を上下に追って数えるしかない。

Home に **running / waiting / failed の状態別件数**を出し、workspace の状況を 1 行で読めるようにする。

## 前提（新しい SSoT を作らない）

件数は既存の daemon 権威 projection から**導出**する。`DaemonMetrics`（mascot sidecar の metrics schema）へ session count field を足さない。metrics は daemon process の観測値であり、session 集合の権威ではないためである。

導出の入力は次の 2 つで、どちらも既に Home へ届いている。

| 入力 | 権威 | TUI での所在 |
|---|---|---|
| session ごとの `SessionLifecycle` | daemon lifecycle snapshot | `ProjectedSession::lifecycle`（`session_lifecycles` から join）／reducer の `AppState::session_lifecycles` |
| session scope の Agent phase 集約 | daemon の Agent phase 報告 | `AppState::phase_for(Target::Session)`（`TargetPhase`） |

## 分類規則

既存語彙（`SessionLifecycle` と `usecase::agent_phase::AgentPhaseAggregation`）だけで決め、新しい状態語彙を増やさない。1 session はちょうど 1 クラスに属するため、3 つの件数の合計が session 数を超えない。

| クラス | 条件 | 理由 |
|---|---|---|
| `failed` | `lifecycle == Failed` | 作成・削除が失敗して**使えない checkout**。lifecycle が唯一の権威で、Agent phase では表せない |
| `waiting` | `failed` でなく、集約が `Waiting` | 少なくとも 1 runtime が入力待ち。人間の介入が要る状態を running より優先して出す |
| `running` | `failed` でなく、集約が `Running` | 少なくとも 1 runtime が実行中 |
| （非計上） | 上記以外 | `Absent` / `Ready` / `Done` |

判断の要点。

- **`failed` は `SessionLifecycle::Failed` だけ**とする。`Creating` / `Initializing` / `Deleting` は進行中の物理状態であって失敗ではなく、`Available` も失敗ではない。
- **`interrupted` / `exited` / `ended` を `failed` に混ぜない**。`AgentPhase::Interrupted` は daemon 再起動後に runtime identity を証明できなかったという **daemon 所有の projection 状態**、`Exited` / `Ended` は正常終了であり、いずれも「checkout が壊れた」という `Failed` とは別の事実である。既存の `AgentPhaseAggregation` はこの 3 つを `Done` に畳んでいるので、その畳み込みをそのまま使い `Done` は非計上とする（#510 の interrupted tab は resume 可能な履歴であり、失敗表示にすると誤解を招く）。
- **precedence は `failed` > `waiting` > `running`**。`Failed` session の runtime は snapshot 更新で落ちるが、順序を明示しておけば競合中の 1 フレームでも二重計上しない。
- 集約の順位付け（`Done > Waiting > Running > Ready > Absent`）は `agent_phase` の既存 rank をそのまま使い、TUI に別順位を作らない。

## 表示

Home の **左 sidebar の mascot sidecar** に 1 行追加する（既存の CPU / memory 行の上）。

- sidecar は rabbit の行に重ねて描くため、**行数（`MascotBlock::reserved_rows`）が増えず** session viewport の容量を奪わない。
- 狭幅では既存規則どおり mascot block ごと省略され、幅が中途半端な場合も既存の clip 規則（`unicode-width` 準拠）に乗る。
- **0 件のクラスは描かない**。3 つとも 0（session 0 件、または全員 idle）なら行自体を出さないので、静かな workspace の frame は現状のまま。
- **metrics unavailable でも出る**。件数は metrics と独立に導出するため、`metrics == None`（daemon 観測が来ていない）でも sidecar に件数行だけが載る。
- 色は theme の既存 role をそのまま使う（running = Success、waiting = Warning、failed = Danger）。

header の右 strip（mode toggle / notice / director）には足さない。あの strip は幅で segment を落とす調整を持ち、並行タスクの変更が集まる場所であるため、衝突を小さくする意図で sidebar 側に置く。

## 受け入れ条件

- running / waiting / failed の件数を Home sidebar に表示し、0 件のクラスは描かない。
- 3 つとも 0 のとき（session 0 件を含む）行を出さず、frame が崩れない。
- `metrics == None` でも件数行が出る。metrics schema（`DaemonMetrics`）に session count field を追加しない。
- 分類が上表どおりであることを test で固定する。特に `Failed` lifecycle と `interrupted` / `exited` / `ended` phase の切り分け、precedence を含む。
- 狭幅 sidebar で mascot ごと省略される／clip される場合に行が溢れない。
- sidecar が 2 行になっても既存 CPU / memory 行の位置が変わらない（1 行だけの現状表示も byte 単位で不変）。
- 分類規則を `document/03-tui.md` に明文化する。
- frame skip（#554）の材料比較に件数が含まれる（`HomeProjection` の一部として比較される）。

## 非目標

- daemon の wire / metrics schema 変更。
- 件数のクリック・絞り込みなどの操作追加（read-only の表示に留める）。
- Welcome / Overview 側の集計表示。
