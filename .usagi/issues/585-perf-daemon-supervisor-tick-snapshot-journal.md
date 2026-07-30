---
number: 585
title: perf(daemon): supervisor tick の冗長な snapshot 二重読込とjournal全再生を無くす
status: done
priority: high
labels: [daemon, performance]
dependson: []
related: []
created_at: 2026-07-30T10:47:19.908125+00:00
updated_at: 2026-07-30T22:41:12.275974+00:00
---

## 背景

`crates/core/src/infrastructure/store/supervisor.rs` の `SupervisorStore::runs()`（L122-151）は、supervisor run のディレクトリを `fs::read_dir` した後、各 run について:

1. まず自分で `json_file::read(&path)` してスナップショットを読む（L143-144）。
2. さらに `self.load(snapshot.supervisor_run_id)`（L146）を呼ぶ。`load()`（L68-76）は**同じスナップショットをもう一度 `json_file::read` し**、そのうえで `self.read_journal(id)` の append-only journal 全件を読み `reduce()` で再生する。

`apply()`（L82-104）は event 追記のたびに reducer 適用済みの完全な snapshot を書き直す（`json_file::write_atomic`）ため、journal は事実上「すでにスナップショットへ反映済みの履歴」を無期限に蓄積するだけで、truncate/rotate されない（`append`/`read_journal` 実装に truncate 処理は存在しない）。したがって `load()` が毎回全 journal を読んで再生する処理は、`reduce` 内部の重複チェック（`crates/core/src/domain/supervisor.rs` の `applied_events` 判定）でほぼ no-op になるだけの完全な空回りであり、run の履歴が増えるほど 1 回の `load()` コストが線形に増える。

この `runs()` は `SupervisorRuntime::tick_all()`（`crates/daemon/src/usecase/supervisor_runtime.rs:357-362`）から呼ばれ、`tick_all` は**daemon 内で Agent PTY が1つ exit するたびに無条件で**呼ばれる（`src/runtime/daemon.rs:2260-2267`）。つまり、無関係な 1 個の Agent exit イベントが daemon 内の**全 supervisor run**について「スナップショット二重読込＋全履歴 journal 再生」を発生させる。

加えて、同じ `tick()`（`supervisor_runtime.rs:384-451`）内で `run.provenance` に含まれる task の数だけ `dispatch_run()`（L453 付近）を呼び、`dispatch_run` は毎回 `self.dispatch.runs()`（`crates/core/src/infrastructure/store/dispatch.rs:322-324`）で dispatch registry ドキュメント全体を読み直す（N+1）。1 tick 内でループの外に持ち出せば1回の read で済む。

## 対象

- `SupervisorStore::runs()` の二重読込を解消する（`load()` を呼ぶ前提なら L143-144 の直接読込を削除する）。
- journal を「snapshot 反映済みの event を truncate/rotate する」設計に変更し、`load()` が読む journal を「snapshot 以降の未反映 event」だけに縮小する。あるいは `tick_all` 側で「exit した Agent runtime に関係する run だけ」を特定して tick するよう変更し、無関係な run への波及を止める。
- `supervisor_runtime.rs::tick()` 内の `dispatch_run` ループを、tick 先頭で dispatch registry を 1 回 load しメモリ上で参照する形に変更する。

## 受入条件

- [ ] 1 回の `load()` が読む journal サイズが、run の全履歴ではなく「未反映分」に比例することを検証するテストがある（または `tick_all` が無関係な run を読まないことを検証するテスト）。
- [ ] `tick()` 1 回あたりの dispatch registry read 呼び出し回数が、run の task 数に比例しない（1 回のみ）ことを検証するテストがある。
- [ ] 既存の supervisor run reconcile・wake 配送の正しさに regression がない（`cargo test -p usagi-daemon` green）。
- [ ] daemon の run 数・履歴が多い状態でも、1 個の Agent exit イベントの処理コストが O(全 run × 全履歴) にならないことが説明できる。
