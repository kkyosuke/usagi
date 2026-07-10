---
number: 181
title: test: slow/flaky test の計測基盤と nextest 採否を検証する
status: todo
priority: medium
labels: [test, perf]
dependson: []
related: []
parent: 177
created_at: 2026-07-10T23:35:22.929986+00:00
updated_at: 2026-07-10T23:35:22.929986+00:00
---

## 背景

2026-07-11 warm 実測: domain 1.96s、infrastructure 51.93s、usecase 55.37s、presentation 18.71s、daemon_ipc integration 11.61〜14.08s、full 104.97s。重さは Git subprocess/temp repo/worktree/submodule、TUI render、daemon/PTY wait に集中する。cargo-nextest は未導入、cargo-llvm-cov 0.8.7 は導入済み。

## 調査

- CI で test ごとの duration/JUnit を artifact 化し、上位 slow tests と run-to-run variance を取得する。
- `cargo test` と cargo-nextest を cold/warm、通常/coverage (`cargo llvm-cov --nextest`) で比較する。
- daemon/PTY test の process cleanup、signal、timeout、capture behavior を nextest 下で反復確認する。
- full suite を複数回走らせ flaky/順序依存を観測する。retry は診断用に限定し required pass を水増ししない。

## 判断基準

wall time が意味のある幅で改善し、coverage 100% と subprocess cleanup が同等なら nextest を CI/ローカルへ段階導入する。改善が小さい場合は依存ツールを増やさず、slow test の fixture/process 起動を個別改善する。hakari は単一 package のため対象外。

## 完了条件

計測表、採否、flaky 一覧（なければ試行回数と none observed）、slow 上位の後続 issue を残す。
