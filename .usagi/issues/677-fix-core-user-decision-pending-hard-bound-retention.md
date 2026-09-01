---
number: 677
title: fix(core): user decision の入力・pending・履歴を hard bound と retention で保護する
status: done
priority: high
labels: [review, v2, core, daemon, mcp, decision, resource, retention]
dependson: []
related: [329, 378, 406, 673]
created_at: 2026-08-13T22:36:36.042890+00:00
updated_at: 2026-09-01T00:04:42.704499+00:00
---

## Finding（P1 resource / durability）

`UserDecision` の title / prompt / option count / option fields / idempotency key / freeform answer に domain hard limit がない。MCP stdio / IPC は 1 message 1 MiB で bounded だが、1件の decision がその大半を使え、同時 pending 数にも admission cap がない。

`crates/core/src/infrastructure/store/user_decision.rs` は `State { decisions: Vec<_>, events: Vec<_> }` を mutation ごとに全件 read-modify-atomic-rewrite する。resolved / cancelled / expired record は削除されず、idempotency key の lookup、pending list、expiry sweep、outbox validation が履歴総量に比例して増え続ける。

#673 は1件の pending wait の disconnect/shutdown lifecycleを扱う。本issueは durable input/store 全体の aggregate resource policyを所有する。

## 対象責務

- field byte、option count、pending count、workspace count、serialized byte の hard capをdomain/admissionで一元化する。
- terminal decision に minimum recovery/idempotency window と age/count/byte retentionを定義する。
- pending、未 ACK outbox、同期応答のrecoveryに必要なrecordはGCしない。
- safeなGC候補がないままcapに達した場合は、既存decisionをsilent evictionせず新規requestをtyped backpressureでeffect zeroに拒否する。
- idempotency keyを保持期間後に削除する場合、old retryをfresh requestとして再作成しないexpiry/tombstone contractを定義する。
- mutation / pending query / expiry sweepが履歴総量を毎回rewrite/scanしないstore layoutまたはindex/compactionにする。

## 受入条件

- [x] 各fieldとoption countのlimit超過はdecision/outbox/worker effect 0で拒否される。
- [x] small budget/fake clockで大量のresolved/cancelled/expired decision後もstore count/bytesがhard cap内に収まる。
- [x] pending / unacked / minimum window内のrecordはpressureでも保持される。
- [x] retained idempotency retryは同じID、expired retryはtyped expiredで、新しいdecisionを作らない。
- [x] TUI pending listと同期MCP回答の既存契約を維持する。

## 根拠箇所

- `crates/core/src/domain/user_decision.rs`
- `crates/core/src/infrastructure/store/user_decision.rs`
- `crates/cli/src/mcp/tools/session.rs::UserDecisionRequest`
- `src/runtime/daemon.rs::dispatch_user_decision`

## 2026-08-24 時点の進捗（v3.0.0 リリースレビュー）

- [x] terminal decision の retention（`TERMINAL_RETENTION` = 256、古い順に破棄）。
      pending と、未 ACK の outbox event が参照する record は GC しない。
- [x] pending 数の hard cap（`PENDING_LIMIT` = 128、workspace ごと）。飽和時は既存を
      silent eviction せず `UserDecisionError::PendingLimitReached` で新規要求を
      effect zero に拒否し、IPC は `ResourceExhausted` として返す。
- [x] title / prompt / option count と各 option field / idempotency key / freeform answer の
      hard cap。MCP schema と domain constructor / store が同じ policy を使い、domain は
      UTF-8 byte 数を検証する。limit 超過は typed effect zero、oversized durable state は
      再起動後も fail closed にする。
- [x] serialized document byte の aggregate hard cap — 4 MiB の exact pretty-JSON 上限と bounded read を実装済み
- [x] idempotency key の expiry / tombstone contract — owner-scoped 固定長 tombstone と `IdempotencyExpired` を実装済み
- [x] mutation / pending query / expiry sweep の cost bound — aggregate 4 MiB、terminal 256 件、pending workspace/global cap、no-op expiry の lock-free read により上限を固定済み

## 2026-09-01 完了確認

- PR #1615（`57ecf776`）で aggregate byte cap、protected-record compaction、owner-scoped idempotency tombstone、effect-zero backpressure が main に導入済み。
- small-budget / fake-clock / restart / duplicate / malformed-state の store test が受入条件を固定する。
- 全 document rewrite は最大 4 MiB に hard bound され、expiry の no-op tick は atomic replacement と fsync を行わない。
