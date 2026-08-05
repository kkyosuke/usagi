---
number: 648
title: fix(daemon): 未 drain の metrics observer queue が MetricsUpdatesDropped を自己誘発する
status: done
priority: medium
labels: [review, v2, daemon, tui, metrics]
dependson: []
related: []
created_at: 2026-08-05T01:01:50.495056+00:00
updated_at: 2026-08-05T08:47:37.288167+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 3。本 issue はその finding を再検証し起票したもの。

## Finding

`crates/tui/src/infrastructure/metrics.rs` の `MetricsHook::connect` は接続ごとに `MetricsAction::Subscribe` を発行し、daemon 側（`crates/daemon/src/usecase/metrics.rs`）に容量 1 の `sync_channel(1)` による `MetricsObserver` を登録する。

しかし本番コードのどこからも `MetricsObserver::try_recv` が呼ばれていない（grep で確認した呼び出し元は `src/runtime/daemon.rs` と `metrics.rs` の unit test のみ）。TUI が実際に表示する metrics は別経路の `MetricsAction::Snapshot` ポーリング（`src/runtime/tui.rs`）で取得しており、subscribe した queue は誰にも drain されない。

結果、`Subscribe` 直後の最初の `publish()` で 1-slot queue が満杯になり、以降の `publish()`（Snapshot ポーリングなどにより約 1 回/秒で発生）は毎回 `TrySendError::Full` となって `dropped_updates` をインクリメントし続ける。

`crates/core/src/usecase/daemon_health.rs` の閾値は `METRICS_UPDATES_PER_SEC = 1` かつ `SUSTAINED_SAMPLES = 3` であり、この自己誘発の増加率（約1/s）はこの閾値に正確に一致する。そのため接続後おおよそ3秒で `HealthReason::MetricsUpdatesDropped`（「更新の取りこぼし」）が必ず点灯する。**daemon が健全であっても毎回この警告が出る。**

## 影響

- health indicator が偽陽性を継続的に出し、本来の目的（実際の劣化を知らせる）を損なう。ユーザーが本物の警告を信頼しなくなるリスク。

## 修正方針（例）

- 使われていない `Subscribe`/push 経路を削除し、Snapshot ポーリングだけに一本化する。
- 経路を残す場合は、subscribe した側が実際に `try_recv` で drain するようにする。

## 受け入れ条件

- 健全な daemon に接続し続けても `dropped_updates` が増加しない（integration test で固定する）。
- 実際に受信側が遅れて queue が埋まるケース（本来の検出目的）は引き続き検出できる。
