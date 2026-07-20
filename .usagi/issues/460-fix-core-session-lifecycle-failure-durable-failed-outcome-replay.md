---
number: 460
title: fix(core): session lifecycle failure を durable failed outcome として replay する
status: todo
priority: high
labels: [review, v2, core, session, durability]
dependson: []
related: [217, 268, 343, 403]
parent: 453
created_at: 2026-07-20T12:06:19.681814+00:00
updated_at: 2026-07-20T12:06:19.681814+00:00
---

## 問題・影響

root/v2 の `crates/core/src/domain/session_lifecycle.rs` では `LifecycleEvent::Failed` が共通 `complete` を通り、`OperationStatus::Succeeded` を保存する。`crates/daemon/src/usecase/session_runtime.rs` と `src/runtime/daemon.rs::dispatch_session` は再送を成功として `Accepted` にし、失敗した create/remove に `session.created` / `session.removed` hook を出せる。

## 成立条件 / 再現フロー

session create/remove の filesystem/Git effect を失敗させ、同じ `OperationId` を同 runtime と restart 後 runtime に再送する。aggregate は `Failed` なのに operation は succeeded と見なされ、effect を再評価せず成功 envelope/hook が生成される。

## 対象責務と非対象

session lifecycle reducer、durable operation outcome、runtime replay、IPC envelope と success hook の整合を対象とする。Agent operation ledger は #458、個別 Git failure の修正は非対象。

## 受入条件

- [ ] `Failed` と interrupted reconcile を durable terminal failure として表現し、success と区別する。
- [ ] 同じ semantic operation の retry/restart は同じ safe failure を replayし、effect count は 1 のままにする。
- [ ] failure replay は `Accepted` や success hook を生成せず、success replay と異 intent conflict は既存契約を維持する。
- [ ] legacy の `session Failed + operation Succeeded` snapshot は failure として保守的に補正する。

## 必須回帰テスト

reducer test、effect failure injection、runtime reopen、composition-level IPC envelope/hook test を create/remove の双方に追加し、retry 前後と restart 前後を固定する。

## docs / 移行影響

`document/04-ipc.md` に outcome/hook matrix、`document/05-daemon.md` に interrupted reconciliation を記載する。legacy snapshot migration は成功を捏造せず、再試行可能性を明示する。
