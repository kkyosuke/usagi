---
number: 473
title: fix(daemon): exited PTY の map entry と FD を回収する
status: in-progress
priority: high
labels: [review, v2, daemon, terminal, resource]
dependson: []
related: [218, 251, 264, 271, 385, 472]
parent: 453
created_at: 2026-07-20T12:06:24.030994+00:00
updated_at: 2026-07-20T21:23:40.365527+00:00
---

## 問題・影響

root/v2 の `src/runtime/daemon.rs` にある `AgentPty.terminals` と `DaemonPty.terminals` は spawn 時に `Arc<PtyTerminal>` を insert するが、observer の exit 処理で remove しない。master/writer/child handle と FD が daemon lifetime 中残り、大量の短命 terminal/Agent で resource exhaustion する。

## 成立条件 / 再現フロー

短命 generic/Agent PTY を多数 spawn/exit し、map size と process FD 数を測る。runtime aggregate が exited でも PTY adapter map が strong reference を保持し続ける。

## 対象責務と非対象

exit observation 後の PTY transport ownership 解放、late command の typed outcome、bounded final replay との境界を対象とする。output bound は #472、pane removal UX、crash 後 FD handoff は非対象。

## 受入条件

- [ ] exit と最後の output を順序どおり registry へ commit 後、map から transport entry を exactly once で除去する。
- [ ] child を reap し master/writer/reader FD を解放し、double exit/detach と race しても leak/panic しない。
- [ ] late input/resize/kill は別 terminal に作用せず typed stale/exited を返す。
- [ ] final replay/tombstone を保持する場合は #472 の byte bound 内で PTY handle と分離する。

## 必須回帰テスト

generic/Agent の大量 spawn/exit、final output race、同時 detach/input/resize、double exit を実 PTY で実行し、map cardinality と FD 数が baseline 近傍へ戻ることを検証する。

## docs / 移行影響

`document/05-daemon.md` に terminal transport と durable/tombstone state の lifetime を記載する。永続/wire migration はない。
