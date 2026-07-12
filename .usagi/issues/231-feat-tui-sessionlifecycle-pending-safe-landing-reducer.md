---
number: 231
title: feat(tui): SessionLifecycle pending と safe landing reducer を実装する
status: in-progress
priority: high
labels: [tui, lifecycle]
dependson: [223, 225, 226]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.263928+00:00
updated_at: 2026-07-12T22:26:35.320995+00:00
---

## 目的

create/remove の pending skeleton、progress/error、interaction counter と safe landing を fake daemon reducer に実装する。

## スコープ

- `OperationId` による pending row、stale/duplicate event の除外、failure rollback。
- 全 input を数える interaction counter と create/remove 成功時の landing policy。

## 対象外

- daemon operation wire、実 subscribe/reconcile、dirty force remove policy。

## Acceptance ID

- `A-LIFE-1` / `A-LIFE-2` の pure/fake slice。

## 依存

- #223/#225/#226。D2 adapter integration は #232。

## 検証

- table-driven fake reducer scenario（accepted/progress/final/failure/stale/interaction）。
