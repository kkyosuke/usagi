---
number: 676
title: fix(core): dispatch registry / inbox を retention と pagination で bounded にする
status: todo
priority: high
labels: [review, v2, core, daemon, mcp, dispatch, resource, retention]
dependson: []
related: [321, 323, 402, 518, 526]
parent: 671
created_at: 2026-08-13T22:32:31.381136+00:00
updated_at: 2026-08-23T23:22:34.965949+00:00
---

## Finding（P1 resource / durability）

`crates/core/src/infrastructure/store/dispatch.rs` の `dispatch.json` は `agents` / `runs` / `bindings` / `admissions` を削除せず、mutation ごとに文書全体を read-modify-atomic-rewrite する。caller inbox も `append_inbox` が JSONL 全件を parse → push → 全体置換するため、N 件の配送で累積 I/O は O(N²) になる。

production `agent_inbox` は全件 load 後に `since` / `unread_only` を filter するだけで、limit、cursor、ACK がない。`mark_inbox_read` は production caller がなく、`unread_only:true` を繰り返しても同じ全メッセージを返す。長寿命 daemon / agent では disk、parse memory、IPC response、registry lookup が履歴総量に比例して増え続ける。

## 対象責務

- registry と caller inbox に count / serialized byte / age の hard cap と minimum replay/idempotency window を定義する。
- live run、current binding、preparing/running admission、未 ACK message は GC しない。
- safe な候補が無いまま hard cap に達した場合、既存 state を silent eviction せず新規 dispatch/report を typed backpressure で effect zero に拒否する。
- `agent_inbox` に bounded page limit、stable cursor、明示 ACK / mark-read semantics を追加する。read と ACK は別 effect とし、response lossで未読を失わない。
- append は全履歴 rewrite を避け、crash-safe append/index/compaction または同等の bounded cost にする。
- run/binding/admission と inbox の cross-reference を壊さず、duplicate completion / NoReport / restart の exactly-once 契約を維持する。

## 受入条件

- [ ] small budget fixture で 10万件相当の terminal run/report を繰り返しても count / bytes が hard cap 内に収まる。
- [ ] page size 1..100 と cursor で全履歴を重複・欠落なく走査でき、1 page の read/parse/response が履歴総量に比例しない。
- [ ] ACK loss、duplicate ACK、restart、同時 append/ACK が同じ unread stateへ収束する。
- [ ] live / preparing / unacked record だけで cap を満たす場合は typed backpressure で、既存recordを削除しない。
- [ ] `agent_inbox {unread_only:true}` が ACK 後に同じmessageを返さない。

## 根拠箇所

- `crates/core/src/infrastructure/store/dispatch.rs::{mutate_registry,append_inbox,inbox,mark_inbox_read}`
- `src/runtime/daemon.rs::dispatch_agent_tool` の `AgentInbox`
- `crates/cli/src/mcp/tools/session.rs::AgentInbox`

## 2026-08-24 時点の進捗（v3.0.0 リリースレビュー）

O(N²) の成長そのものを止める retention を入れた。

- [x] registry の finished run に `RUN_RETENTION`（256）の上限。live（`Preparing` /
      `Running`）は年齢によらず保持し、binding / admission は所属 run に追従する。
      Agent は履歴ではなく `upsert_agent_by_runtime_model` が再利用する記録なので
      eviction 対象にしない（外すと relaunch のたびに `AgentId` が変わる）。
- [x] caller inbox の read message に `INBOX_READ_RETENTION`（256）の上限。unread は
      read より常に優先して保持し、`INBOX_HARD_LIMIT`（4096）超過時だけ最古の unread を
      落として error log に記録する（silent loss にしない）。append 1 回の cost が
      履歴総量に比例しなくなり、累積 O(N²) が O(N) になった。
- [ ] `agent_inbox` の bounded page limit / stable cursor / 明示 ACK — **未対応**。
      retention とは別の API 変更で、この issue に残るスコープはこれである。
- [ ] 全履歴 rewrite を避ける crash-safe append/index/compaction（上限付き rewrite で
      cost は bounded になったが、append そのものは依然 rewrite である）
