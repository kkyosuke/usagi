---
number: 673
title: fix(daemon): pending user decision wait を切断・shutdown aware にする
status: todo
priority: high
labels: [review, v2, daemon, mcp, decision, lifecycle, availability]
dependson: []
related: [329, 406, 557, 658]
parent: 671
created_at: 2026-08-13T00:13:57.985707+00:00
updated_at: 2026-08-13T00:13:57.985707+00:00
---

## Finding（P1 availability / lifecycle）

`src/runtime/daemon.rs::wait_for_user_decision` は `user_decision_request` の同期応答を実現するため、25 ms ごとに `user-decisions.json` を読み直す無期限 loop で `Pending` の解決を待つ。`expires_at` は任意であり、loop は client disconnect、generation retirement、daemon shutdown を一切観測しない。

1 call が accepted socket と `usagi-ipc-client` worker を保持する。client が先に切断しても handler は socket IO に戻らないため worker は残る。shutdown/rollover は `ClientWorkers::retire` で socket を shutdown して全 worker を join するが、この worker は socket を見ずに polling を続けるため retirement が完了しない。期限なし request を繰り返すと bounded connection slots も枯渇する。

## 修正方針

- pending wait を store polling ではなく notification/cancellation-aware な wait port にする。
- decision resolve/cancel/expire の state transition、client disconnect、daemon/generation shutdown のいずれでも waiter を起こす。
- disconnect/shutdown では worker/connectionだけを解放し、durable Pending recordを暗黙回答・削除しない。再接続後は get/list または同じ idempotency key で観測できる。
- pollingを残す場合も disk read cadence、絶対 deadline、shutdown cancellationを明示的に bounded にする。ただし固定25 ms sleepをworker数だけ増やさない。

## 受入条件

- [ ] `expires_at` なしで回答待ち中の client を切断すると、decisionはdurableに残り、client worker/connection slotは bounded time 内に解放される。
- [ ] 同じ状態で daemon shutdown / generation retirement が全 client worker を bounded time 内に join できる。
- [ ] resolve/cancel/expire は待機中の元 call を一度だけ起こし、既存の同期回答契約を維持する。
- [ ] N 件 pending でも idle read/fsync/wakeup rate が N × 40/s にならない。
- [ ] restart、duplicate idempotency key、late resolve、foreign owner は既存の fail-closed 契約を保つ。

## 根拠箇所

- `src/runtime/daemon.rs::wait_for_user_decision`
- `src/runtime/daemon.rs::start_ipc_accept_loop`
- `crates/daemon/src/usecase/authority/workers.rs::ClientWorkers::retire`
