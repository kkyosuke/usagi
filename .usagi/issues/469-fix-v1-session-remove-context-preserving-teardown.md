---
number: 469
title: fix(v1/session): remove を context-preserving な二段階 teardown にする
status: done
priority: high
labels: [review, v1, session, durability]
dependson: []
related: [49]
parent: 453
created_at: 2026-07-20T12:06:22.694501+00:00
updated_at: 2026-07-20T22:13:01.725597+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/session/mod.rs::remove` は Git teardown より先に conversation、phase、PR、prompt queue、pane などの回復 context を消す。`list_repo_worktrees` や `discard_session` が部分失敗すると worktree/session は残るのに retry・診断情報が失われる。

## 成立条件 / 再現フロー

multi-repo session の一部 worktree を dirty/locked にして remove を実行する。teardown error 後に session metadata と ancillary state を確認すると、作業 tree を修復するための context が先に削除されている。

## 対象責務と非対象

remove の prepare→teardown→commit cleanup transaction、durable tombstone/retry、ancillary cleanup 順序を対象とする。reconcile の所有権判定は #470、force policy 自体の変更は非対象。

## 受入条件

- [ ] destructive context cleanup は全 managed Git teardown 成功後に commit するか、再開可能な durable tombstone を先に保存する。
- [ ] 途中失敗で conversation/phase/PR/queue/pane と session identity を保持し、安全に retry できる。
- [ ] retry は既に消えた component を idempotent に扱い、unmanaged data を消さない。
- [ ] partial state と次の利用者 action を明示 error/status で投影する。

## 必須回帰テスト

locked/dirty worktree、multi-repo の途中 failure、ancillary cleanup failure、process crash 各点の failpoint で context 保持、retry 完了、二重削除なしを検証する。

## docs / 移行影響

v1 session removal/recovery docs に intermediate state と retry 手順を追記する。既存 orphan state は保守的に pending removal として reconcile し、自動 force delete しない。
