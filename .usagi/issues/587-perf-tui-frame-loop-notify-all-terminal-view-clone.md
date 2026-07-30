---
number: 587
title: perf(tui): frame loop の無条件 notify_all と不要な terminal_view clone を無くす
status: todo
priority: medium
labels: [tui, performance]
dependson: []
related: []
created_at: 2026-07-30T10:47:50.727899+00:00
updated_at: 2026-07-30T10:47:50.727899+00:00
---

## 背景

`document/03-tui.md` は metrics lane について「request rate は frame rate に比例しない」という不変条件を明言している。しかし2つの点で、この精神に反する「毎フレームの無駄な処理」が残っている。

### 1. `RefreshPump::activate()` が状態変化の有無を無視して毎回 `notify_all()` する

- `src/runtime/refresh_pump.rs:316-321`:
  ```rust
  pub fn activate(&self) {
      lock(&self.shared.state).activate();
      self.shared.signal.notify_all();   // 内部状態が変化していなくても常に notify
  }
  ```
  内部の `RefreshState::activate`（`refresh_pump.rs:162-167`）は `due.is_none()` のときだけ状態を変える idempotent 設計で、コメントも「Idempotent, and cheap enough for a caller that reaches it once per frame」と明言しているが、外側の `RefreshPump::activate` はこの idempotency を無視して毎回 `notify_all()` する。
  呼び出し元は `crates/tui/src/presentation/mod.rs` の frame loop（`metrics_backend.poll(&metrics_sessions)`）経由で、workspace 表示中は毎 tick（約60Hz、idle 時も）無条件に呼ばれる。
  対照的に `TerminalInventoryPump::watch`（`src/runtime/inventory_pump.rs:468-475`）は「実際に変化したときだけ signal する」規律（`if changed { self.signal(); }`）を守っており、metrics lane だけがこの規律から外れている。
  影響: metrics 常駐 worker thread が実際のフェッチ間隔（1秒）と無関係に、frame tick と同じ約60Hzで condvar wake → mutex lock → 即再park を繰り返す。

### 2. `terminal_view.clone()` が毎 tick、稀にしか使わない Config 分岐のためだけに発生

- `crates/tui/src/presentation/mod.rs:5178` — `home_frame_material(..., terminal_view.clone(), ...)` が毎 tick 呼ばれる。
- `crates/tui/src/presentation/mod.rs:5281` — Config 画面を開いている稀なケースだけで使う `render_controller_frame(..., terminal_view.clone(), ...)`。
- `home_frame_material` が `terminal_view: Option<TerminalViewProjection>` を値で consume するため、L5281 のために元の値を残す目的で L5178 が clone している。`TerminalViewProjection.rows` は viewport 行数分の `String` を保持するため、毎 tick 数十件の heap alloc + copy が発生する（99%以上のケースで L5281 の分岐は使われない）。

## 対象

- `RefreshState::activate` が「dormant → active へ実際に遷移したか」を返すようにし、`RefreshPump::activate` はその戻り値が true のときだけ `notify_all()` する（`TerminalInventoryPump::watch` と同じパターンに揃える）。
- L5281 の Config 分岐でだけ `terminal_view` を再計算する、または `home_frame_material` を `&Option<TerminalViewProjection>` 受け取りに変更し、不要な毎tick clone を無くす。

## 受入条件

- [ ] 状態変化がない場合、`RefreshPump::activate()` が `notify_all()` を呼ばないことを検証するテストがある（既存の `an_idle_lane_request_count_follows_the_cadence_not_the_frame_rate` はフェッチ数のみ検証しており、この観点をカバーしていない点に注意）。
- [ ] `terminal_view` の clone が Config 分岐を開いている場合以外で発生しないことを確認する（ベンチマークまたはコードレビューで示す）。
- [ ] `cargo test -p usagi-tui --bin usagi` および root の frame loop 関連テストが green。
- [ ] 既存の metrics 表示・Config 画面表示の挙動に regression がない。
