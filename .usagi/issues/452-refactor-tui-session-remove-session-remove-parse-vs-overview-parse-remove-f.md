---
number: 452
title: refactor(tui): session remove 文法の二重実装（session_remove::parse vs overview の parse_remove、-f 対応差あり）を一本化する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:05:04.201364+00:00
updated_at: 2026-07-20T12:05:04.201364+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/usecase/session_remove.rs:22-34` — `parse()` はトークンループで `-f | --force` を受け、**重複指定を明示拒否**（:27-28 `"force flag must not be repeated"`）。
- `crates/tui/src/usecase/overview/mod.rs:217-236` — `parse_remove()` はスライスパターンで **`--force` のみ受け付け（`-f` 非対応）**、`-s`/`--select` を追加サポート。

## 問題

同じ「session remove」コマンドの文法が入口によって違う（`-f` が効いたり効かなかったり、重複 `--force` が拒否されたりされなかったり）。ユーザーから見て一貫性がなく、文法変更が 2 箇所修正になる。

## 改善案（要検討）

- `parse_remove` を `session_remove::parse` へ委譲し、`-s/--select` は overview 側の拡張として合成する。
- 受理する文法（`-f` 対応・重複拒否・select）を 1 箇所で定義し、両入口のテストで固定する。

## 受け入れ条件

- [ ] remove 文法のパーサが 1 実装になり、`-f`/`--force`/重複/`-s` の挙動が両入口で一致する。
- [ ] coverage 100% を維持する。
