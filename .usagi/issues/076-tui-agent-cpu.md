---
number: 76
title: TUI: agent プロセスの CPU/メモリ使用量を表示（行ごと + ウサギ右に合計）
status: in-progress
priority: medium
labels: [feat, tui]
dependson: []
related: []
created_at: 2026-06-27T10:03:42.495875+00:00
updated_at: 2026-06-27T10:03:50.380813+00:00
---

## 目的

usagi が起動している agent/shell プロセス群の CPU・メモリ使用量を TUI ホーム画面に表示する。

## 表示内容

- **各セッション行**: そのセッションで live な PTY/agent プロセスツリーの CPU% とメモリ（例 `CPU 8%  120MB`）。プロセスのない行は空欄（live な行のみ表示）。一覧 2 行目の右側に配置。
- **合計**: 全 live プロセスツリーの合計 CPU% / メモリを左下ウサギの右に表示（例 `CPU 23%  MEM 512MB`）。ウサギが引っ込んでいる時は非表示。

## 実装方針

- domain: `ResourceUsage` 値型 + 合算・人間可読フォーマット（純粋ロジック、100% テスト）。
- infrastructure: `ResourceSampler` トレイト + `sysinfo` 実装（ルート PID 群→プロセスツリー使用量）。実 IO は coverage 除外・注入可能に。
- `PtySession::process_id()` を公開。
- pool の既存 200ms 監視スレッドに相乗りし、worktree→PID を集め 1〜2 秒間隔でサンプリング。live プロセスが無ければサンプリングしない（idle 最適化を維持）。
- `HomeState` にスナップショット保持、`panes.rs`/`ui/mod.rs` で描画。skip-paint に「resource changed」条件を追加。

## ドキュメント

- `document/design/05-home.md`（行のリソース表示・ウサギ右の合計）
- `document/02-architecture.md`（sysinfo 依存追加）
