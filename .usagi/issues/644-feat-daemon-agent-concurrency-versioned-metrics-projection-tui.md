---
number: 644
title: feat(daemon): Agent concurrency の「使用中/上限」を versioned metrics projection で公開し TUI に表示する
status: done
priority: medium
labels: [daemon, tui, ipc, metrics, agent]
dependson: []
related: []
created_at: 2026-08-04T12:57:11.598612+00:00
updated_at: 2026-08-04T13:40:55.833152+00:00
---

## 背景

daemon は Agent runtime の同時起動数に上限を持ち、上限に達した launch を `ConcurrencyExhausted` で拒否する。
判定の正本は `RuntimeCoordinator`（`crates/daemon/src/usecase/runtime.rs`）の
`occupied_slots() >= limit` であり、`limit` は `AGENT_RUNTIME_LIMIT`（`crates/daemon/src/usecase/agent_ipc.rs`）
から composition 時に与えられる。

ところがこの「いま何枠使っていて上限は幾つか」は **どの client にも公開されていない**。TUI は mascot 足元の
sidecar に daemon metrics（CPU / memory）を出しているが、Agent concurrency は出せないため、ユーザーは
Agent を起動しようとして拒否されるまで枠が埋まっていることを知れない。

## 目的

daemon の admission が実際に使う concurrency 権威をそのまま projection として公開し、TUI に「使用中/上限」を
簡潔に表示する。TUI 側で件数を推測したり `AGENT_RUNTIME_LIMIT` を複製したりしない。

## 語彙（混同しないもの）

| 語 | 正本 | 本 issue の対象 |
|---|---|---|
| Agent concurrency | `RuntimeCoordinator.limit` と `occupied_slots()` | **これ** |
| generic terminal capacity | `GENERIC_TERMINAL_LIMIT` / `CapacityPolicy::limit(ResourceKind::Terminal)` | 対象外 |
| 全体 capacity pool（durable allocator） | `AllocatorDocument::pool_used` / `CapacityPolicy` | 対象外（Agent pool の限度値は同じ定数から来るが、表示は in-process owner の admission 状態を出す） |
| supervisor run の `ExecutionPolicy.max_concurrency` | `usagi-core` の `domain::supervisor` | 対象外 |

**active（使用中）の定義**は admission が数えるものと同一にする。すなわち `occupied_slots()` が数える
`Reserved` / `Running` / `ReconcileRequired(_)` の record であり、`Exited` / `Reclaimed` / `SpawnFailed` は数えない。

## 設計

1. **権威からの publish（daemon）**: `AgentConcurrencyGauge`（lock-free）を追加し、`RuntimeCoordinator` が
   durable mutation の唯一の choke point（`persist()`）と gauge の bind 時に `occupied_slots()` / `limit` を
   publish する。値は権威の accessor から導出するだけで、定数を別に持たない。
2. **metrics 経路は block しない**: metrics は表示専用の lossy 経路であり、daemon の進行や他 observer を
   block してはならない（`document/05-daemon.md` の metrics observer 契約）。よって metrics dispatch は
   Agent runtime の mutex を取らず、`MetricsBroker` に bind した gauge を lock-free に読む。
   broker はすでに subscriber / drop count の権威なので、Agent concurrency も同じ位置で wire snapshot に載せる。
3. **versioned projection（IPC）**: `DaemonMetrics` に `agent_concurrency: { in_use, limit }` を optional
   object として追加し、`schema_version` を 2 → 3 にする。used と limit を 1 つの object にまとめることで、
   別 tick の used と limit を組み合わせて読むことが構造上できない。field 欠落は `None`（旧 daemon）、
   未知 field は無視（旧 client）で、rollover 中の版跨ぎでも壊れない。
4. **TUI 表示**: mascot sidecar に 1 行追加して `使用中/上限` を出す。0、上限到達（強調）、projection 不在、
   狭幅 clip を扱い、TUI 側の閾値判定は core の projection type が持つ述語を使う。

## 受け入れ条件

- daemon が Agent を 1 つ起動している間、metrics snapshot の `agent_concurrency.in_use` が 1、`limit` が
  `AGENT_RUNTIME_LIMIT` になる。spawn 失敗・exit 後は in_use が戻る。
- metrics dispatch は Agent runtime の lock を取らない（取れば launch 中の metrics tick が待たされる）。
- `schema_version` が 3 になり、`agent_concurrency` 欠落の payload は `None` として読める。未知 field を含む
  payload も読める。
- TUI の mascot sidecar に「使用中/上限」が出る。projection 不在時は 0 と区別できる表示になり、
  metrics 全体が無いときは従来の `waiting daemon` のまま。狭い pane では既存どおり mascot ごと省略される。
- `document/04-ipc.md`（schema table）・`document/05-daemon.md`（metrics observer / Agent concurrency の定義）・
  `document/03-tui.md`（sidebar mascot の表示）を同じ PR で更新する。

## テスト

- core: `AgentConcurrency` の round-trip、legacy（field 欠落）、forward（未知 field）、飽和判定。
- daemon: gauge の bind / publish / 未 publish、broker snapshot への反映、launch → spawn 失敗 → exit での
  in_use 遷移。
- root: metrics dispatch が bind 済み gauge の値を返し、Agent runtime lock を取らないこと。
- tui: sidecar の present / 0 / 上限到達 / 不在 / 狭幅。
