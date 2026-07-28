---
number: 573
title: feat(daemon): draining generation を claim が 0 になった後だけ自動回収する
status: in-progress
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: [572]
related: [516, 518, 526, 559, 572]
parent: 559
created_at: 2026-07-27T22:58:20.258958+00:00
updated_at: 2026-07-28T20:56:16.756758+00:00
---

## 問題・根拠（コード調査で確定）

`authority::rollover::collect_retired` は実装済みだが production の呼び出し元が無い（grep で 0 件。
同名の `usecase::resources::durable::collect_retired` は shard file の掃除であり別物）。

したがって [#572](572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md) が handoff を起動できるように
なっても、**draining generation は永久に残る**。process、socket、registry entry、capacity claim が
解放されないため、次の rollover は generation 上限に当たる。

## この issue を分けた理由

回収は「最後の claim が 0 になったことをどう観測するか」が本体であり、handoff の起動
（[#572](572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)）とは失敗の仕方が違う。handoff は
「始めてよいか」を間違えると二重 active を作り、回収は「終わってよいか」を間違えると**生きている PTY を
落とす**。混ぜると、壊れたときにどちらの判定が原因か切り分けられない。

## 既存 issue との境界

- [#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)（done）—
  `collect_retired` の順序（発行停止 → 0 確認 → `retired` → worker join → registry 記録）の正本はそちら。
  **判定を再実装しない**。
- [#526](526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md)（done）— final tombstone の
  retention budget は対象外。
- [#572](572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md) — handoff の起動。本 issue は
  「handoff 後の draining process が自分を終わらせる」ところだけを持つ。

## やること

1. draining generation の process に回収 worker を置く。回収してよい条件は
   **owned resource・lease・outbox・capacity claim がすべて 0** であり、そのどれか 1 つでも残っていれば
   回収しない。
2. 条件が満たされたら `collect_retired`（gate + `ClientWorkers` + registry）を実行し、endpoint を retire して
   process を exit する。
3. 回収の待ち方は fake 側の観測に載せる。固定 sleep で代用しない
   （[背景 worker を残したままテストを終えない](../../document/06-conventions.md#背景-worker-を残したままテストを終えない)）。

## 設計上の判断が必要な点

- **0 の観測元**。owner shard（自 generation の resource / in-flight command / outbox）と global allocator
  （capacity claim）は別 document である。両方を読んで「両方 0」を確かめる瞬間に片方が増えうるので、
  lease と同じ「発行を止めてから 0 を待つ」形にできるかを決める。
- **回収できないまま残った draining**。PTY が長時間生き続ける場合、draining process はその間ずっと残る。
  これは正しい（PTY を守るのが目的）が、generation 上限と衝突する。上限に当たったときに
  「回収待ちの draining が居る」ことを typed に報告する必要がある。
- **crash した draining**。回収前に SIGKILL された draining の registry entry は
  `activation::reclaim_dead_generations` が回収する（#516 / #1331）。その経路で十分か、
  capacity claim の解放も必要かを決める。

## 受入条件

- [ ] old generation は最後の resource / lease / outbox / capacity claim 終了後**だけ**自動回収される。
- [ ] 回収は endpoint・process・registry entry をこの順で解放し、保持した client worker を全 join してから
      endpoint を回収する。
- [ ] restart 後の新規 Agent / generic Terminal は new active が所有し、old resource の exit は durable
      state / capacity へ**一度だけ**反映される。
- [ ] G1 の exit と G2 の spawn を同時実行しても lost update が無い（#562 の契約が回収経路でも保たれる）。
- [ ] generation 上限に当たったときは、回収待ちの draining が居ることを typed に報告して fail closed になる。
- [ ] 回収 worker を残したままテストを終えない。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の
      [admission fence](../../document/05-daemon.md#admission-fence) と
      [generation と orphan safety](../../document/05-daemon.md#generation-と-orphan-safety) を更新する。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::authority::rollover` / `usecase::resources` が退行しないこと）
- `cargo test -p usagi --bin usagi`（回収 worker の配線）
- claim を 1 つずつ落として「最後の 1 つが残っている間は回収しない」ことを固定する。
- capacity claim だけが残るケース、outbox だけが残るケースを個別に固定する。
- Rust 差分を含むため fmt / check / clippy / 推奨 test を通し、full gate は PR CI で確認する。
