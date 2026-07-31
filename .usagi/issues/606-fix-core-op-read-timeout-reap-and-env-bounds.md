---
number: 606
title: fix(core): op read timeout で child を terminate/reap し env 解決を bounded にする
status: todo
priority: medium
labels: [review, v2, core, env, process, resilience, resource, security]
dependson: []
related: [500, 538]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-07-31T15:00:00+09:00
---

## Finding（P2 resource）

`crates/core/src/infrastructure/env_resolver.rs::op_read` は `wait_with_output` を detached thread で行い、30秒の `recv_timeout` 後に child を kill/reap せず thread も回収しない。`resolve_parallel` は `EnvBindings` の secret reference 数だけ thread/process を同時生成し、global/workspace settings の binding 数・同時解決数にも上限がない。hung `op` を多数指定すると launch ごとに process/thread/pipe が残り続ける。

## 最小修正方針

child handle の owner が timeout 時に terminate→bounded wait→kill→reap する cancellation-safe runner を用意する。binding 数、secret reference 数、同時 `op` 数を domain/load/admission の一貫した上限で拒否し、queue も bounded にする。

## テストと受け入れ条件

- hung fake `op` は timeout 後に exact child が reap され、stdout/stderr reader thread と FD が残らない。
- 上限超過 settings は保存または launch admission で安全に拒否され、process を一つも spawn しない。
- bounded concurrency 下でも literal 値、成功 secret、個別 failure の既存 merge policy が維持される。
