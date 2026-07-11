---
number: 183
title: feat(orchestration): durable plan・claim・reconcile state machine を追加する
status: todo
priority: high
labels: [orchestration, infrastructure]
dependson: []
related: []
parent: 182
created_at: 2026-07-10T23:56:41.533909+00:00
updated_at: 2026-07-10T23:56:41.533909+00:00
---

## 背景

DAG の判断を agent の会話だけに置くと、親停止や再起動で進行と retry 情報を失う。

## やること

- workspace-local な orchestrator plan/state/event store を stamped envelope、atomic write、lock/CAS で実装する。
- node state、attempt/generation、lease、deadline、next retry、worker/base/PR を表現する。
- main の issue ready とは別に work-ready を派生し、純粋な reconcile が冪等 action を返すようにする。
- `(workspace, issue)` claim と lease 回収前の session/PR 再観測を実装する。

## 受け入れ条件

- 二つの owner の claim は一方だけ成功する。
- delegating 前後の crash、同一 snapshot の再適用、stale lease が二重委譲しない。
- 既存 issue ready テストが不変である。
- 保存形式と状態遷移のドキュメントを追加する。
