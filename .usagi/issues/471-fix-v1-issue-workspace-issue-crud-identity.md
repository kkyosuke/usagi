---
number: 471
title: fix(v1/issue): workspace 全体で issue 番号と CRUD identity を一意にする
status: done
priority: high
labels: [review, v1, issue, concurrency, durability]
dependson: []
related: [23, 335, 404]
parent: 453
created_at: 2026-07-20T12:06:23.376287+00:00
updated_at: 2026-07-20T21:15:58.618026+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/issue/mod.rs::create` は各 worktree 固有 lock の下で workspace 全体を scan して採番するため、別 session/process が同じ番号を予約できる。`v1/src/infrastructure/issue_store.rs::{read,write_locked,remove}` は同番号 sibling を任意に読み、update/delete で全 sibling を消す。現 backlog に #323 と #390 の同番号ファイルが実在し、誤読・破壊の危険が現実化している。

## 成立条件 / 再現フロー

2 worktree/process を barrier で同時 create すると同番号の異 filename が作れる。重複を seed して read/update/delete すると選択が非決定的または sibling 全削除になる。

## 対象責務と非対象

v1 CLI/MCP の workspace-global reservation、番号 identity、duplicate detection と安全な CRUD、sequence migration を一つの不変条件として対象にする。root/v2 allocator #335 の status 変更、既存 #323/#390 の本 triage PR での renumber は非対象。

## 受入条件

- [ ] common Git/workspace authority の atomic sequence/reservation で process/worktree 間の番号を一意にする。
- [ ] duplicate number の read/update/delete は typed ambiguity で fail closed し、どの sibling も暗黙に変更しない。
- [ ] stale/missing/corrupt sequence は最大既存番号から lock 内で安全に移行し、予約済み番号を再利用しない。
- [ ] 明示的な repair/migration 手順が exact file identity を提示する。

## 必須回帰テスト

複数 process/worktree barrier で create 一意性を検証し、seeded duplicate の read/update/delete sibling 保持、sequence の stale/missing/corrupt migration、crash 後予約を固定する。

## docs / 移行影響

v1 issue CLI/MCP docs に ambiguity error と repair 手順を記載する。#323/#390 は履歴参照を監査した別 migration で解消し、本 issue 実装が任意選択してはならない。
