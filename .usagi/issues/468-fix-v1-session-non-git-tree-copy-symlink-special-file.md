---
number: 468
title: fix(v1/session): non-Git tree copy で symlink と special file を安全に扱う
status: done
priority: high
labels: [review, v1, session, security, filesystem]
dependson: []
related: [49, 250]
parent: 453
created_at: 2026-07-20T12:06:22.373929+00:00
updated_at: 2026-07-20T21:18:30.860029+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/session/tree.rs::build_dir` は directory 以外をすべて `fs::copy` へ渡す。file symlink は外部 secret を dereference して session tree へ複製し、directory/dangling symlink は失敗し、FIFO/device/socket は block または未定義動作になる。

## 成立条件 / 再現フロー

non-Git source に外部 sentinel への file/dir symlink、dangling symlink、FIFO、Unix socket を置いて session を作る。外部 bytes の複製、hang、部分作成された session を観測できる。

## 対象責務と非対象

non-Git tree builder の file type policy、symlink 非 dereference、special file の bounded reject、失敗時 rollback を対象とする。Git worktree の symlink semantics と repository discovery（#250）は非対象。

## 受入条件

- [ ] `symlink_metadata` 等で regular file/directory/symlink/special を effect 前に分類する。
- [ ] symlink は外部 content を dereference せず、明示 policy に従って link 自体を再現または拒否する。
- [ ] FIFO/device/socket 等は block せず typed error で拒否し、部分 session を残さない。
- [ ] source tree 外への traversal と destination escape を canonical/relative policy で防ぐ。

## 必須回帰テスト

external-secret file symlink、directory/dangling symlink、FIFO の wall-clock bound、socket/利用可能な special file、通常 file/dir、途中 failure rollback を real filesystem test で固定する。

## docs / 移行影響

v1 non-Git session copy semantics を docs に記載し、既存 session に dereference 済み secret が含まれる可能性を security note として案内する。既存 tree の自動削除はしない。
