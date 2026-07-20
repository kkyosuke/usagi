---
number: 441
title: refactor(core): store のロック規律を LockedJsonStore に一本化し、user_decision の失敗時 fsync 書き込みを解消する
status: todo
priority: medium
labels: [refactor, core, infra, review]
dependson: []
related: []
created_at: 2026-07-20T12:02:33.794536+00:00
updated_at: 2026-07-20T12:02:33.794536+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/core/src/infrastructure/store/` にロック規律が 2 流派混在し、強制機構がない:

- **内部ロック型**（各 mutate メソッド内で `StoreLock` 取得）: dispatch.rs（:240/:271/:288）・user_decision.rs（:147）・lifecycle.rs（:83/:100/:121/:149）・supervisor.rs（:96）。memory.rs / issue.rs は内部取得に加えて `pub fn lock()` も公開。
- **呼び出し側ロック型**（caller が `lock()` を保持）: `WorkspaceStateStore`（state.rs:56）・`Storage`（workspace.rs:40、`pub fn lock()` :84）。

また `user_decision.rs` の `mutate`（:145-151）は、遷移クロージャが失敗（`Err(Terminal)`）や no-op でも**無条件に** `json_file::write_atomic(...)`（:150）で毎回 fsync 書き込みする。

## 問題

- どちらの流派に従うべきか新規 store で判断できず、「ロックを通らない変異 API が存在しない」ことを誰も保証していない。
- lock→load→mutate→write の同型実装が 3 系統以上重複。
- 失敗/no-op でも fsync することで、decision の高頻度ポーリング系操作が不要なディスク書き込みを発生させる。

## 改善案（要検討）

- lock→load→mutate→write を `LockedJsonStore<T>` として `persistence/` に集約し、変異 API がロックを通らない形を型で保証する。
- mutate は「状態が変化した時だけ書く」契約（クロージャが変更有無を返す）に変更する。

## 受け入れ条件

- [ ] store のロック規律が 1 抽象に集約され、同型 3 実装が解消されている。
- [ ] user_decision の no-op/失敗経路で書き込みが発生しないことがテストで固定されている。
- [ ] coverage 100% を維持する。
