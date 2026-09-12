---
number: 736
title: feat(daemon): workflow を daemon の常駐 lane で進める
status: done
priority: high
labels: [v2, daemon, workflow, agent]
dependson: []
related: [737, 738, 739]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T05:00:00+00:00
---

## 問題

workflow の reconcile（peer message からの phase 遷移）、queued 指示の再配送、PR 検証は
`DaemonRequest::WorkflowSnapshot` / `WorkflowControl` の処理内でしか走らない。その request を出すのは
TUI の tick polling だけで、条件は「active session の Workflow panel」である。

| 状況 | 現在起きること |
|---|---|
| 別 session を選択中 | その workflow は reconcile されない |
| TUI を閉じている | Agent は動き続けるが `Verifying → Ready` が来ない |
| 指示の宛先が一時的に不在 | queued のまま再配送されない |

state は message journal の cursor replay で後から追いつくので壊れないが、**「投げて放っておく」が成立しない**。
watchdog（#738）も通知（#739）も、この pull 駆動のままでは作れない。

## 方針

- daemon 側に常駐 workflow lane を置き、active run（`Ready` / 終了済みを除く phase）ごとに低頻度
  （5〜15 秒間隔）で reconcile + queued 指示の配送 + PR 検証を回す。
- lane は既存の常駐 worker と同じ規約に従う: 合成時ではなく駆動時に観測を開始し、停止時は worker を
  残さずに終了する。
- TUI の polling は **UI 更新のための読み取り**へ格下げし、進行の責任を持たせない。snapshot request は
  引き続き最新状態を返す。
- 1 request の中で `synchronized_snapshot` が最大 4 回走っている現状を、1 request 1 reconcile に整理する。
- lane の駆動は workspace ごとの active run 集合に限定し、run が無い workspace では wake しない。

## 受入条件

- [x] TUI を閉じた状態でも active run の reconcile・queued 指示の配送・PR 検証が進む。
- [x] 非 active session の run も同じ間隔で進む。
- [x] run が無い間の 1 tick のコストは workflow record の列挙だけで、Agent 観測も durable write も行わない。daemon 停止時に worker を残さない。
- [x] 1 回の workflow request で reconcile が複数回走らない。
- [x] TUI の polling を止めても進行が続くことを示す test がある。
- [x] `document/05-daemon.md` と `document/03-tui.md` の「進行は snapshot 取得時に行う」記述を更新する。
