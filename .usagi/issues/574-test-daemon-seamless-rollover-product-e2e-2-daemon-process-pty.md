---
number: 574
title: test(daemon): seamless rollover の product E2E を 2 daemon process と実 PTY で固定する
status: done
priority: high
labels: [review, v2, daemon, lifecycle, test]
dependson: [572, 573]
related: [508, 516, 559, 572, 573]
parent: 559
created_at: 2026-07-27T22:58:43.948747+00:00
updated_at: 2026-07-28T23:44:51.304835+00:00
---

## 問題・根拠

[#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) は必須 product E2E を要求している。
[#572](572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md) と
[#573](573-feat-daemon-draining-generation-claim-0.md) の unit / integration test は
「判定が正しいこと」を固定するが、**shipping binary・2 個の実 daemon process・別 Unix socket・実 PTY child**
でなければ出ないものが残る。

既に近い足場がある。`tests/cli_tui.rs` は `spawn_serve` / `spawn_standby`（`--standby`）/ `registry_document` /
`kill_and_reap` で 2 process を扱い、`crates/daemon/tests/generation_authority.rs` は**テスト専用の**server pair で
handoff の crash matrix を固定している。後者は product ではないので、product 側の同じ検証がこの issue の対象である。

## この issue を分けた理由

重い E2E は CPU を占有するため直列化が要る
（[重い E2E の直列化](../../document/06-conventions.md#重い-e2e-の直列化)）。実装 PR に混ぜると、
落ちたときに「product の失敗」と「CPU 競合による timeout」の区別がつかない。E2E は単独でレビューし、
直列化と reap を単独で検証できる形にする。

## やること

`tests/support/daemon.rs` の command builder 経由で起動する
（[結合テストからの daemon 起動](../../document/06-conventions.md#結合テストからの-daemon-起動)）。既存の
daemon 起動 lock と同じ lock を取る。

次を固定する。

- live Agent / generic Terminal あり・なし
- readiness failure、registry / locator 各 write 境界の SIGKILL recovery、observable commit の非 rollback
- routing capability 無し / 旧 client / revision mismatch で handoff effect zero
- persistent old connection、in-flight spawn / control、internal background producer
- TUI close / reopen、active locator 切替、draining endpoint 一時不通後も old owner ref へ再接続
- restart response / ACK loss、concurrent / repeated restart、generation limit
- G1 exit と G2 spawn の同時実行、late / duplicate old event、capacity release

restart 前後の active / draining PID と generation、Agent / generic child PID と OS start identity、
spawn count を記録する。**provider resume argv が一度も実行されないこと**を child の identity で示す。

## 受入条件

- [ ] 上記すべてを product E2E で固定する。
- [ ] 実 PTY / 実 daemon を使う test は既存の lock で直列化し、teardown で exact reap する。
- [ ] タイミングで決まる事象は観測できるまで loop を駆動し、観測できない run は上限で失敗させる
      （skip が起きなかった run を pass と読み替えない）。
- [ ] 背景 worker を残したまま test を終えない。
- [ ] カバレッジ 100% を維持する。

## 必須回帰テスト・計測

- `cargo test -p usagi --test <target>`
- full gate は PR CI で確認する。
