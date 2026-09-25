---
number: 765
title: refactor(core,daemon,tui): 5 種の時刻 port を統一し、同名 port trait の衝突を解消する
status: done
priority: low
labels: [v2, refactor, core, daemon, tui]
dependson: []
related: [762]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T14:15:03.289094+00:00
---

## 問題

時刻・待機を表す port trait が 5 つある。

| trait | 場所 |
|---|---|
| `MonotonicClock` | `crates/core/src/infrastructure/client.rs` |
| `Sleeper` | `crates/core/src/infrastructure/daemon/mod.rs` |
| `RefreshClock` | `crates/daemon/src/usecase/pr_inventory.rs` |
| `RetentionClock` | `crates/daemon/src/usecase/terminal_retention_ipc.rs` |
| `LogicalClock` | `crates/daemon/src/usecase/resources/retention.rs` |

それぞれに `FakeClock` が別実装で存在し（#762）、production 側も `SystemClock` / `SystemLogicalClock` /
`ProductionRefreshClock` / `RealSleeper` を合成ルートで個別に束ねている。

また `crates/tui/src/usecase/application` には `SessionCommandPort` が 2 つある（`daemon_backend.rs` の
`create / refresh / remove / sleep` port と `runtime_ports.rs` の `execute` port）。同名で意味が違うため、import 先を
見ないと読めない。

## 方針

- monotonic / wall / logical の 3 語彙に整理し、core の `domain`（または protocol 隣接の共通 module）に置く。
  daemon の `RefreshClock` / `RetentionClock` は同じ trait への alias か、必要な最小 interface へ収束させる。
- `SessionCommandPort` の一方を用途を表す名前（例: `SessionLifecycleCommands` / `SessionActionExecutor`）に改名する。
- fake は core の `test_support` に 1 実装ずつ置く。

## 完了条件

- 時刻 port trait が 3 つ以下。
- workspace 内で同名の public trait が無い。
