---
number: 484
title: test(coverage): coverage(off) の allowlist と CI enforcement を定義する
status: in-progress
priority: medium
labels: [review, v2, coverage, test]
dependson: []
related: [356, 360, 380]
parent: 453
created_at: 2026-07-20T12:06:49.167064+00:00
updated_at: 2026-07-20T23:00:35.148400+00:00
---

## 問題・影響

root/v2 には `#[coverage(off)]` が約 854 箇所あり、`document/06-conventions.md` が許す真の IO/composition/generic 重複だけでなく reducer、parser、error/reconcile path にも付いている。100% gate を保っても business regression と未配線 production path を隠せ、新規 exclusion の増加を CI が止めない。

## 成立条件 / 再現フロー

business branch を壊しても対象 function 全体が coverage 集計外なら gate は緑になる。現状は除外理由の機械可読 allowlist/期限/owner がなく、review だけで規約を強制できない。

## 対象責務と非対象

root/v2 の exclusion policy、理由付き allowlist、CI lint、領域別返済順序を対象とする。具体的な core/daemon/TUI 除外除去は #485/#486/#487、coverage 率の引き下げ、v1 frozen code の一括変更は非対象。

## 受入条件

- [ ] 許可理由を real IO/composition と generic monomorphization 重複に限定し、fake/integration test の代替を要求する。
- [ ] path/symbol/reason/owner または inline reason の検証可能な allowlist を定義する。
- [ ] 新規・未登録・期限切れ `coverage(off)` を CI が失敗させ、fixture で lint 自体を検証する。
- [ ] 既存 854 箇所を分類し、#485〜#487 以外の許可/削除先も一覧化する。

## 必須回帰テスト

許可された IO、禁止された reducer、理由欠落、stale symbol、追加/削除 fixture で lint pass/fail を固定し、既存 100% coverage gate と同時に実行する。

## docs / 移行影響

`document/06-conventions.md` と CI docs に allowlist workflow、例外 review、返済手順を追記する。runtime/data migration はない。
