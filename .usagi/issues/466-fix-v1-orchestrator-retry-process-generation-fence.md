---
number: 466
title: fix(v1/orchestrator): retry を process と generation で fence する
status: in-progress
priority: high
labels: [review, v1, orchestration, durability]
dependson: []
related: [182, 183, 184, 186]
parent: 453
created_at: 2026-07-20T12:06:21.701940+00:00
updated_at: 2026-07-20T22:12:33.488872+00:00
---

## 問題・影響

出荷中 v1 の retry は deterministic な既存 worker session を再利用し、旧 Agent process を停止せず `v1/src/infrastructure/orchestrator_event.rs::register` の mutable generation binding を上書きする。旧 process の `emit_with_liveness` が event 時点の新 binding を読み、新 generation として completion/failure を発行できる。

## 成立条件 / 再現フロー

worker generation 1 を生かしたまま retry で generation 2 を登録し、旧 process から event を送る。旧 process が新 generation identity を借用し、new run を誤完了させる。

## 対象責務と非対象

retry 前の process termination/confirmation、immutable generation credential/binding、stale event rejection を対象とする。claim の排他は #465、Agent CLI 自体の resume 機能追加は非対象。

## 受入条件

- [ ] retry は旧 worker を停止・reap して確認するか、generation 固有 session/process identity を使ってから新 spawn する。
- [ ] process が保持する generation identity は immutable で、registry の後書き換えを event provenance に使わない。
- [ ] stale generation event は durable に拒否・記録し、新 run state を変更しない。
- [ ] restart 後も active/retired generation と process liveness を保守的に reconcile する。

## 必須回帰テスト

live old worker→retry→旧 event、新 worker event、stop failure、即 exit、daemon/reconcile restart を含む integration test で generation 誤帰属と二重 process がないことを検証する。

## docs / 移行影響

v1 retry/cancel docs に process と generation の境界を記載する。旧 mutable binding は active と推定せず stale/unknown として移行する。
