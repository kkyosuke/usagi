---
number: 455
title: perf: 小粒パフォーマンス改善の束（毎フレーム確保・supervisor journal O(n²)・filtered() 再計算ほか）
status: todo
priority: low
labels: [perf, review]
dependson: []
related: []
created_at: 2026-07-20T12:05:40.887384+00:00
updated_at: 2026-07-20T12:05:40.887384+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。60Hz ループ・ホットパスの小粒な無駄を束ねる。

## 根拠と改善案（要検討・検証済み）

1. `crates/tui/src/presentation/mod.rs:1579-1591` — `sync_runtime_sessions` が毎フレーム `session_ids().to_vec()`（:1580）と全 `name.clone()` の Vec（:1585-1591）を構築して `!=` 比較。→ 世代カウンタ or 参照比較へ。
2. `presentation/mod.rs:1530` — `project_controller_sessions` が毎フレーム `Vec<ProjectedSession>` を `.collect()` で再構築。→ 変更時のみ再構築。
3. `presentation/mod.rs:1887-1888` — `runtime.overview_modal().cloned()` / `closeup_modal().cloned()` が毎フレーム modal 状態を clone。→ 参照描画へ。
4. `presentation/metrics.rs:71`（および `views/workspace.rs:613`）— `metrics()` が clone 返し。→ 参照返し or Arc。
5. `presentation/mod.rs:260-262` — `key_to_terminal_bytes` が `Key` を値渡しし、`Key::Passthrough(bytes)` で `bytes.clone()`（ホット分岐で 1 clone）。→ 参照渡し化。
6. **supervisor store の O(n²)**: `crates/core/src/infrastructure/store/supervisor.rs:90-98` — `apply()` が毎回 `load()` → `read_journal()` → `reduce()` で**journal 全再生**（:77-79）。`append()`（:154）に compaction がなく、apply のたびに伸びた journal を全読みするため O(n²)。さらに `domain/supervisor.rs:443` の `reduce()` が受理イベントごとに `run.clone()`。→ 状態スナップショット＋増分 reduce、または定期 compaction。
7. `crates/tui/src/presentation/views/open.rs:314` — `filtered()` が lowercase 変換込みで再計算され、1 フレーム内から複数回（:454, :460 ほか）呼ばれる。→ フィルタ結果のメモ化。

## 受け入れ条件

- [ ] 7 項それぞれについて改善または見送り理由の記録がされている。
- [ ] 挙動が回帰しない（既存テスト維持）。coverage 100% を維持。
