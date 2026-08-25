---
number: 485
title: test(core): business reducer と persistence error path を coverage 対象へ戻す
status: done
priority: medium
labels: [review, v2, core, coverage]
dependson: []
related: [356, 360, 380]
created_at: 2026-07-20T12:06:49.488265+00:00
updated_at: 2026-07-21T00:24:44.315651+00:00
---

## 問題・影響

## 成立条件 / 再現フロー

`crates/core/src/domain/session_lifecycle.rs` や `crates/core/src/infrastructure/persistence/markdown_store.rs` 等の excluded branch を変更して coverage report を比較しても、未実行 branch/function が gate に影響しない。

## 対象責務と非対象

## 受入条件

- [ ] core の reducer、validation、replay、cache decision、error mapping から規約外 exclusion を除く。
- [ ] IO は port/fake/failpoint で decision logic と分離し、error path を deterministic にテストする。
- [ ] workspace 100% gate を維持する。

## 必須回帰テスト

session success/failure/replay/conflict、Markdown source/derived/freshness、store corrupt/schema/IO failure を branch table で実行し、coverage lint/report が対象 symbol を含むことを検証する。

## docs / 移行影響

テスト設計上の port/failpoint を開発 docs に追記する場合だけ更新する。production behavior/data migration はない。
