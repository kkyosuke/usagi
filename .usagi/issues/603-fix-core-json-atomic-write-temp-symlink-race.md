---
number: 603
title: fix(core): JSON atomic write の temp symlink race を排除する
status: done
priority: high
labels: [review, v2, core, persistence, security, filesystem]
dependson: []
related: [461, 511, 515]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-07-31T22:23:12.740873+00:00
---

## Finding（P1 security）

`crates/core/src/infrastructure/persistence/json_file.rs::unique_tmp_path` は PID と process-local counter から予測可能な名前を作り、`write_atomically_inner` は `std::fs::File::create` で開く。store directory を書ける別主体が次の temp path を symlink として先置きすると、`File::create` が link target を truncate/write するため、store 外の daemon 権限で書ける file を破壊できる。`create_new`、no-follow、opened identity の検証がない。

## 最小修正方針

同一 directory に十分ランダムな temp を `create_new(true)` と platform の no-follow 相当で作成し、既存 node との衝突は再試行する。rename 前後の crash durability は維持し、失敗時 cleanup は自分が作成した inode だけに限定する。

## テストと受け入れ条件

- 予測した temp 名に regular file / symlink を置く adversarial fixture でも target 外 sentinel は不変である。
- collision は既存 node を truncate せず別名へ再試行する。
- concurrent writer、write/rename failpoint、fsync、atomic replace の既存契約が維持される。
