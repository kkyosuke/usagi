---
number: 465
title: fix(v1/orchestrator): workspace claim を production admission で原子的に使う
status: todo
priority: high
labels: [review, v1, orchestration, concurrency]
dependson: []
related: [182, 183, 185]
parent: 453
created_at: 2026-07-20T12:06:21.362921+00:00
updated_at: 2026-07-20T12:06:21.362921+00:00
---

## 問題・影響

出荷中 v1 には `v1/src/infrastructure/orchestrator_store.rs::{claim,release_claim,load_claims}` があるが、`v1/src/usecase/orchestrator.rs::reconcile_workspace_tick` など production dispatch は使わない。複数 usagi process/session が同じ issue を同時に claim せず dispatch し、二重 worker と競合する PR を作れる。

## 成立条件 / 再現フロー

同じ workspace/issue を 2 coordinator process で同時に ready にし barrier 後に tick する。両者が admission を通り、同じ logical work を別 process が開始できる。

## 対象責務と非対象

workspace+issue 単位の atomic claim、production admission、release、crash/restart recovery を対象とする。worker generation の process fencing は #466、dependency base は #464、v2 supervisor scheduling は非対象。

## 受入条件

- [ ] dispatch/reconcile の spawn 前に shared workspace authority で atomic claim を取得する。
- [ ] 1 issue に同時に有効な owner は 1 つで、loser は副作用なく観測可能な busy/claimed outcome を返す。
- [ ] success/failure/cancel で release し、crash 後は liveness/lease 契約により安全に reclaim する。
- [ ] claim owner/generation を durable に保存し、別 workspace の同番号 issue と混同しない。

## 必須回帰テスト

2 process/barrier integration test で spawn count 1、loser effect 0、release/retry、owner crash、stale lease、restart recovery を検証する。

## docs / 移行影響

v1 orchestration docs に claim key、lease/recovery、利用者に見える busy 状態を記載する。既存 claim file は schema/version を検証し、曖昧なら自動 dispatch しない。
