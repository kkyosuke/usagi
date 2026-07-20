---
number: 417
title: fix(tui): decisions.refresh の 16ms 毎 tick・毎回新規 daemon 接続をキャッシュ付き port に改める
status: todo
priority: high
labels: [fix, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:56:27.585496+00:00
updated_at: 2026-07-20T11:56:27.585496+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/presentation/mod.rs:2320-2328` — `if matches!(key, Key::Other)`（約 60 回/秒の tick で真）のたびに `decisions.refresh(workspace_id)` を呼ぶ（:2321。初回 refresh は :2263）。
- 本番 port `DaemonDecisionCommandPort::refresh`（`src/runtime/tui.rs:125`）は呼び出しごとに `Self::client()`（:109-112 → `crate::runtime::daemon::client(ClientPolicy::tui())`）で**毎回新規 daemon 接続＋同期往復**する。
- 対照的に metrics 側は port が 1 秒キャッシュを所有するパターン: `src/runtime/tui.rs:213-215`（`sample.elapsed() < Duration::from_secs(1)`、git 側 :266）。

## 問題

アイドル中でも毎秒約 60 回、Unix socket の接続確立＋handshake＋同期往復が発生する。daemon とクライアント双方の CPU を浪費し、daemon 停止時は毎 tick 接続失敗のレイテンシが UI スレッドに乗る。

## 改善案（要検討）

- metrics port と同じ「port がキャッシュを所有」パターンに揃える（例: 1 秒キャッシュ＋変更時のみ reducer へイベント）。
- あわせて接続の使い回し（persistent client）も検討する。

## 受け入れ条件

- [ ] tick ごとの新規接続・同期往復が解消され、refresh 頻度が明示的な間隔（例: 1 秒）に律速される。
- [ ] pending decision の表示遅延が仕様として明記され、テストで固定されている。
- [ ] coverage 100% を維持する。
