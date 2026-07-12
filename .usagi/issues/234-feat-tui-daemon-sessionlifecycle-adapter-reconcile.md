---
number: 234
title: feat(tui): daemon SessionLifecycle adapter と reconcile を結合する
status: done
priority: high
labels: [tui, ipc, lifecycle]
dependson: [217, 219, 220, 231]
related: []
parent: 227
created_at: 2026-07-12T21:11:47.427108+00:00
updated_at: 2026-07-12T23:02:21.421032+00:00
---

## 目的

D2 の accepted/progress/final、snapshot/replay、reconnect reconcile を lifecycle reducer へ配線する。

## スコープ

- `OperationId` と revision/sequence を保つ daemon client adapter。
- disconnect 後の operation list/subscribe/snapshot reconcile と durable intent の重複防止。

## 対象外

- daemon reducer/API の実装、TUI-local lifecycle policy の再設計。

## Acceptance ID

- `A-LIFE-1` / `A-LIFE-2` の D2 結合。

## 依存

- #217/#219/#220 と #231。

## 検証

- fake IPC integration と socket scenario で reconnect/stale/duplicate/reconcile を確認する。
