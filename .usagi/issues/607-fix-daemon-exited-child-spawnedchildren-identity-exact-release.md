---
number: 607
title: fix(daemon): exited child の SpawnedChildren identity を exact release する
status: done
priority: low
labels: [review, v2, daemon, process, resource, lifecycle]
dependson: []
related: [473, 518, 550]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-08-02T14:14:46.375406+00:00
---

## Finding（P3 leak）

`src/runtime/daemon.rs::SpawnedChildren::observe` は PID→`ChildIdentity` を insert するが、terminal/Agent observer の exit・spawn failure・retention release 時に exact identity を remove する経路がない。短命 child を繰り返すと daemon lifetime 中 map が単調増加し、PID reuse 後も古い proof を保持する。

## 最小修正方針

spawn/observe から RAII registration または `(pid,start_identity,process_group)` を用いた exact release token を返し、全 exit/failure path で解放する。PID だけの remove は新 incarnation を消し得るため禁止する。

## テストと受け入れ条件

- 大量の短命 terminal/Agent 後に registry size が baseline に戻る。
- 同じ PID の古い release が新 identity を消さない。
- running child の durable ownership verification は exit 観測まで維持される。
