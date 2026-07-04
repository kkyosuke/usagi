---
number: 76
title: TUI: agent プロセスの CPU/メモリ使用量を表示（行ごと + ウサギ右に合計）
status: done
priority: medium
labels: [feat, tui]
dependson: []
related: []
created_at: 2026-06-27T10:03:42.495875+00:00
updated_at: 2026-07-04T04:22:11.694138+00:00
---

## 目的

usagi が起動している agent/shell プロセス群の CPU・メモリ使用量を TUI ホーム画面に表示する。

## 表示内容

- **各セッション行**: そのセッションで live な PTY/agent プロセスツリーの CPU% とメモリ。プロセスのない行は空欄（live な行のみ表示）。一覧 2 行目の右側に配置。表示は Nerd Font アイコン（microchip / memory グリフ）で CPU%・メモリを示す。
- **合計**: 全 live プロセスツリーの合計 CPU% / メモリを左下ウサギの右に表示。ウサギが引っ込んでいる時は非表示。

## 実装

- domain: `ResourceUsage` 値型 + `ProcSample` + `aggregate_by_root` + `Load` バンド（純粋ロジック、単体テスト済み）。`src/domain/resource.rs`
- infrastructure: `ResourceSampler` トレイト + `sysinfo` 実装 `SysinfoSampler`。`src/infrastructure/resource.rs`
- `PtySession::process_id()` を公開。`src/infrastructure/pty.rs`
- pool の監視スレッド（200ms）に相乗りし、live プロセスがあるときのみ 10 tick ≒ 2 秒間隔でサンプリング（`RESOURCE_SAMPLE_EVERY = 10`）。live プロセスが無ければサンプリングしない（idle 最適化を維持）。`src/presentation/tui/home/terminal/pool.rs`
- `HomeState` にスナップショット保持、`ui/panes.rs`・`ui/mod.rs` で描画（行の `resource_inline_label` / ウサギ右の `total_beside_mascot`）。

## ドキュメント

- `document/design/05-home.md`（行のリソース表示・ウサギ右の合計）
- `document/02-architecture.md`（sysinfo 依存追加）
