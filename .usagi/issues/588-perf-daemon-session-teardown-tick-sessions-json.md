---
number: 588
title: perf(daemon): session teardown tick の毎秒 sessions.json 全体再読込を無くす
status: in-progress
priority: low
labels: [daemon, performance]
dependson: []
related: []
created_at: 2026-07-30T10:48:00.313792+00:00
updated_at: 2026-07-30T23:13:01.777652+00:00
---

## 背景

`src/runtime/daemon.rs:1812-1851` の session teardown worker loop は `SESSION_TEARDOWN_TICK = 1s` で常時稼働し、削除待ちが 0 件でも永久にティックし続ける。

各 tick で `drain_pending_teardowns` が `journal.pending()` を呼び、これは `SessionRuntime::state()` → `DaemonLifecycleStore::load()`（`crates/core/src/infrastructure/store/lifecycle.rs:52-54,159-161`）経由で `sessions.json`（workspace の全 session のライフサイクル・operation journal を含むドキュメント）を毎回 disk read + full JSON parse する（`session_runtime.rs:902-922` の `pending_teardowns`）。session teardown の受理契約自体（`Deleting` 由来で明示的な queue を持たない設計）は `document/05-daemon.md` で意図的な設計と説明されているが、その実現手段として「1秒ごとに sessions.json 全体を disk から再読込する」ことまでは文書上で正当化されていない。

同種の設計（`crates/core/src/infrastructure/store/user_decision.rs:76-89` の decision maintenance）は「read はするが lock/write は避ける」という前提を明示コメントで示し、対象ドキュメントも小さいことを踏まえた判断だが、session teardown 側にはこの前提（読み取りコストが小さいこと）を保証する記述がなく、session 数・履歴が多い workspace ではコストが線形に増える常時ポーリングになる。

## 対象

- `WorkspaceLifecycleState` を daemon プロセス内でメモリキャッシュし、`sessions.json` の書き込み（`apply` / `replace_if_revision`）をトリガに更新する形へ変更する、または
- `pending_teardowns` 用に「`Deleting` 行の有無」だけを安価に判定できる別経路（例: 削除要求発生時にだけ立てるフラグ、または軽量なインデックス）を用意する。

いずれの場合も、削除待ちが無い定常状態では 1 秒毎の full read+parse が発生しないようにする。

## 受入条件

- [ ] 削除待ちが 0 件の定常状態で、1 秒毎に `sessions.json` の full read+parse が発生しないことを検証するテストがある。
- [ ] 削除待ちが発生した場合の検出・処理に regression がない（既存の teardown 受理契約を維持する）。
- [ ] `cargo test -p usagi-daemon` が green。
