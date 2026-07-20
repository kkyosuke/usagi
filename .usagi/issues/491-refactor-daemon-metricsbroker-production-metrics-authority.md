---
number: 491
title: refactor(daemon): MetricsBroker を production metrics の唯一の authority にする
status: todo
priority: medium
labels: [review, v2, daemon, metrics]
dependson: []
related: [297, 319]
parent: 453
created_at: 2026-07-20T12:06:51.474490+00:00
updated_at: 2026-07-20T12:06:51.474490+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/metrics.rs::MetricsBroker` は unit test 済みだが production constructor から使われず、`src/runtime/daemon.rs::ProcessMetrics` / `dispatch_metrics` が別の subscriber/state 実装を持つ。`active_subscribers` と `dropped_updates` が 0 固定になる経路もあり、documented metrics と実挙動が乖離する。

## 成立条件 / 再現フロー

production daemon に複数 subscriber、slow subscriber、publish/drop を発生させて metrics snapshot を取得する。test 済み broker の count/backpressure ではなく ad hoc runtime 値が返り、constructor usage search でも broker は test のみである。

## 対象責務と非対象

production metrics authority を `MetricsBroker` または同等の 1 実装に統合し、重複 `ProcessMetrics` を削除する。新しい product metric の追加、external telemetry backend は非対象。

## 受入条件

- [ ] production composition が唯一の broker instance を生成・共有する。
- [ ] subscribe/unsubscribe/publish/backpressure/drop count と snapshot が同じ authority を使う。
- [ ] ad hoc counters、hard-coded 0、test-only duplicate を除去する。
- [ ] daemon restart/connection close で subscriber lifecycle と metrics が整合する。

## 必須回帰テスト

production composition で複数/slow subscriber、disconnect、drop、snapshot、restart を実 IPC か同等 harness で検証し、unit broker と同じ値を返すことを固定する。

## docs / 移行影響

metrics contract を `document/05-daemon.md` に必要に応じ追記する。wire field を維持し、data migration はない。
