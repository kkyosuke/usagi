---
number: 428
title: test(core): issue store の git common-dir 解決（allocation_dir / git_dir_from_dot_git）を純関数化してテストする（除外かつ未テスト）
status: todo
priority: high
labels: [test, core, review]
dependson: []
related: []
created_at: 2026-07-20T11:59:06.732766+00:00
updated_at: 2026-07-20T11:59:06.732766+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/core/src/infrastructure/store/issue.rs` — `allocation_dir`（:299-331、`#[coverage(off)]` :298）と `git_dir_from_dot_git`（:336-356、attr :335）。
- 両関数とも**coverage 除外かつテストゼロ**（参照は本番呼び出しのみ: allocation_dir 自呼び :189-191、git_dir_from_dot_git は allocation_dir:311 から）。

## 問題

この 2 関数は「worktree から issue 採番するとき、番号カウンタを git common dir（メインリポジトリ側）に解決する」主目的分岐で、壊れると**複数 worktree での issue 番号重複**が起きる。usagi の並行 session 運用（このリポジトリ自身の運用形態）が直撃する経路なのに、テストで守られていない。`.git` ファイルの `gitdir:` 行パースという純粋な文字列処理が実 IO と絡んで除外されている。

## 改善案（要検討）

- `gitdir:` 行のパース・common dir 導出を純関数に抽出し、worktree/通常リポジトリ/壊れた `.git` ファイルの各ケースをユニットテストで固定する。
- 実 IO（fs 読み取り）は薄いアダプタに残し、`#[coverage(off)]` はそこだけに付ける。
- tempfile による worktree 実体を使った integration test も 1 本置く。

## 受け入れ条件

- [ ] common-dir 解決ロジックが純関数としてテストされ、coverage 除外は実 IO 部分のみになる。
- [ ] worktree からの採番がメインと同じカウンタを共有することがテストで固定されている。
- [ ] coverage 100% を維持する。
