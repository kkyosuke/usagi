---
number: 128
title: refactor(tui): terminal/pool.rs（1430 行）からバックグラウンド監視・PR スキャンを分離する
status: done
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-04T23:16:46.409578+00:00
updated_at: 2026-07-04T23:16:46.409578+00:00
---

## 背景（なぜ問題か）

`home/terminal/pool.rs` は 1430 行で、2 つの異なる責務が同居している。(A) ペインのライフサイクル管理（`TerminalPool`/`SessionPanes`/`Pane`、タブ操作、スナップショット）と、(B) バックグラウンド監視系（`MonitorHandle`/`MonitorSnapshot`/`Shared`、`spawn_watcher`、PR スキャン `PrScanJob`/`scan_pr_jobs`/`pending_pr_scans`/`persist_pr_results`/`apply_pr_results`、`deliver_live_prompts`、`notify`）。PR スキャンは vt100 スクリーンから URL を抽出して `pr_link_store` に永続化する独立フローで、ペイン操作 API とはロック（`Shared` mutex）を介してしか関わらない。ペイン操作の変更と監視ロジックの変更が同じファイルに集中している。

## 対象箇所

`src/presentation/tui/home/terminal/pool.rs`: `Watched`/`WatchedPrPane`/`LivePromptTarget`/`Shared`/`MonitorHandle`/`MonitorSnapshot`/`snapshot_locked`/`PrScan*`/`spawn_watcher`/`deliver_live_prompts`/`notify`。

## やること

- 監視系を `home/terminal/monitor.rs`（`MonitorHandle`/`Shared`/`spawn_watcher`/通知）と `home/terminal/pr_scan.rs`（`PrScanJob`/`scan_pr_jobs`/`persist_pr_results`/`apply_pr_results`）に切り出す。
- `TerminalPool` は監視ハンドルを保持する形を維持する。（`pr_link_store` への依存は presentation→infrastructure で方向は正。層違反ではなくファイル肥大・責務混在の是正が目的。）

## 受け入れ条件

- `pool.rs` が 800 行以下になる。監視/PR スキャンのユニットテストが新モジュールに移り通過する。
- `TerminalPool` の公開 API 無変更。カバレッジ 100% 維持。
