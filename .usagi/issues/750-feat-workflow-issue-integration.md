---
number: 750
title: feat(workflow): issue から workflow を起こし、PR と done まで繋ぐ
status: todo
priority: high
labels: [v2, workflow, issue, pr]
dependson: [749]
related: [745]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

現在は人間が毎回、issue を読み、goal を書き写し、PR 本文に `Internal-Issue:` を書き、マージ前に
issue を `done` にする、という手順を踏んでいる。usagi は issue store も PR 監視も持っているのに、
workflow はそのどちらとも繋がっていない。

## 方針

- issue 番号から workflow を起こせるようにする。goal は issue 本文から生成し、参照を run に保持する。
- 実装 Agent への初期指示に「PR 本文へ `Internal-Issue: #<番号>` を書く」「PR を開く前に issue を
  `done` にする差分を同じ PR に載せる」という既存の規約を含める。
- `Ready` 到達時に、issue の status が `done` になっているか（= 規約どおりの差分が PR に載っているか）を
  検証し、欠けていれば待ち理由として出す。
- issue の書き込みは従来どおり session worktree からのみ行い、root のチェックアウトを汚さない。

## 受入条件

- [ ] issue 番号を指定して workflow を起こせ、goal が issue 本文から生成される。
- [ ] run が参照 issue を保持し、snapshot に含まれる。
- [ ] `Ready` の検証に `Internal-Issue` と issue status の整合が含まれる。
- [ ] issue の書き込みが session worktree 経由に限定される。
- [ ] `document/03-tui.md` と issue 運用のドキュメントを更新する。
