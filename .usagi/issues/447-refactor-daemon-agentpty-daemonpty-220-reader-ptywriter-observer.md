---
number: 447
title: refactor(daemon): 合成ルートの AgentPty / DaemonPty ~220 行重複（reader スレッド・PtyWriter・observer）を統合する
status: todo
priority: high
labels: [refactor, daemon, review]
dependson: []
related: [423, 432]
created_at: 2026-07-20T12:03:59.395079+00:00
updated_at: 2026-07-20T12:03:59.395079+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `src/runtime/daemon.rs` の `AgentPty`（enum observer :440、struct :448、impl PtySpawner :468、impl PtyWriter :536）と `DaemonPty`（struct :569、impl GenericPtySpawner :587、impl PtyWriter :646）は、reader スレッドのループ・`wait()` による exit 処理・`select_terminal`/`resize`/`write_all` が**変数名以外ほぼ同一**（~220 行、span ~:440-680）。
- 双方が `terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>`（:449, :570）を持つ。

## 問題

PTY まわりの修正（例: #423 の PtyWriter API 変更、reader の wait ロック解消）が常に 2 箇所同時修正になり、片側だけ直す事故が起きる。

## 改善案（要検討）

- spawner trait の差（PtySpawner vs GenericPtySpawner）だけを残し、reader スレッド・writer・observer 通知を単一実装（generic または合成）に統合する。
- #423（PtyWriter API 変更）と同時に実施すると二度手間がない。#432（daemon.rs 移設）とも同じファイルを触るため実施順を調整する。

## 受け入れ条件

- [ ] PTY の reader/writer/observer 実装が 1 系統になる。
- [ ] agent/terminal 両面の PTY 挙動（起動・出力配信・exit 検知・resize）が回帰しない。
- [ ] coverage 100% を維持する。
