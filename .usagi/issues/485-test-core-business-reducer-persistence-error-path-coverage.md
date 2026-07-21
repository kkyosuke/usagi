---
number: 485
title: test(core): business reducer と persistence error path を coverage 対象へ戻す
status: in-progress
priority: medium
labels: [review, v2, core, coverage]
dependson: [484]
related: [356, 360, 380]
parent: 453
created_at: 2026-07-20T12:06:49.488265+00:00
updated_at: 2026-07-20T23:32:32.486432+00:00
---

## 問題・影響

root/v2 core の domain reducer、usecase、persistence error path に function-level `#[coverage(off)]` があり、session lifecycle failure (#460) や Markdown partial failure (#477/#478) のような business invariant を 100% gate が観測しない。

## 成立条件 / 再現フロー

`crates/core/src/domain/session_lifecycle.rs` や `crates/core/src/infrastructure/persistence/markdown_store.rs` 等の excluded branch を変更して coverage report を比較しても、未実行 branch/function が gate に影響しない。

## 対象責務と非対象

core domain/usecase/persistence の business/error branch を test seam と共に coverage 対象へ戻す。real filesystem/clock/process の薄い adapter は #484 policy に従い理由付きで残せる。daemon/TUI は #486/#487。

## 受入条件

- [ ] core の reducer、validation、replay、cache decision、error mapping から規約外 exclusion を除く。
- [ ] IO は port/fake/failpoint で decision logic と分離し、error path を deterministic にテストする。
- [ ] 残る exclusion は #484 allowlist の理由と integration test を持つ。
- [ ] workspace 100% gate を維持する。

## 必須回帰テスト

session success/failure/replay/conflict、Markdown source/derived/freshness、store corrupt/schema/IO failure を branch table で実行し、coverage lint/report が対象 symbol を含むことを検証する。

## docs / 移行影響

テスト設計上の port/failpoint を開発 docs に追記する場合だけ更新する。production behavior/data migration はない。
