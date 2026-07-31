---
number: 602
title: fix(daemon): 保存済み session name を再検証し teardown を session root 内へ拘束する
status: in-progress
priority: high
labels: [review, v2, daemon, session, security, persistence, filesystem]
dependson: []
related: [511, 543]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-07-31T21:38:35.152547+00:00
---

## Finding（P0 security）

`crates/core/src/infrastructure/store/lifecycle.rs` の `DaemonLifecycleStore::load_persisted` は `sessions.json` を deserialize するだけで `WorkspaceLifecycleState::validate` や session name の path 検証を行わない。`crates/daemon/src/usecase/session_runtime.rs` の `SessionRuntime::pending_teardowns` は保存済み `ManagedSession.name` を `SessionRuntime::session_root` で join し、`crates/daemon/src/infrastructure/session_worktree.rs` の `SystemSessionWorktreeIo::remove_session_tree` がその path を `remove_dir_all` する。

session Agent に渡す Claude sandbox の writable roots には `src/runtime/daemon.rs::claude_writable_roots` が daemon durable state の base を含める。Agent が `data_home/daemon/sessions.json` の Deleting record を absolute path または `..` を含む name に改ざんし daemon を再起動すると、join が session directory 外を指し、teardown worker が任意 directory を再帰削除できる。

## 最小修正方針

- durable state の全 read 境界で state invariant と canonical session name を検証し、不正 document は fail closed にする。
- teardown effect の直前にも defense in depth として、解決先が canonical session container の直下であり repository root / data home / filesystem root ではないことを確認する。
- durable state を Agent writable root から外すことも権限分離として検討するが、保存値の検証を代替させない。

## テストと受け入れ条件

- absolute name、`../victim`、separator、symlink ancestor を含む `sessions.json` の restart fixture が拒否され、victim sentinel が残る。
- 正常な中断 teardown は restart 後も再開できる。
- `pending_teardowns` が返す全 target は session container 直下に拘束され、検証失敗時に Git / filesystem effect が一度も呼ばれない。
