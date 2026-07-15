---
number: 308
title: fix(tui): Session mode では右ペインを非アクティブ表示にする
status: todo
priority: medium
labels: [tui, ui, parity]
dependson: []
related: [302]
created_at: 2026-07-15T00:28:01.690978+00:00
updated_at: 2026-07-15T00:28:01.690978+00:00
---

## 目的

Session mode（session の選択・切替を行う面）では、右ペインを操作対象ではないことが分かる dim 表示にする。Closeup / live tab の active 表示と混同させない。

## 受け入れ条件

- Session mode 中の右ペイン全体（tab strip を含む）が一貫して dim になり、左ペインの選択が主操作であることを示す。
- Closeup / live terminal / modal 合成時の style precedence を定義し、active tab や modal の可読性を損なわない。
- session 切替、pending / live / empty pane、resize、ANSI reset 後も style leak がない。
- renderer test / golden test で Session mode と Closeup mode の明度差を固定する。

## 関連

#302 は Switch の左 sidebar 非選択 row を dim にする修正であり、右ペインの mode-aware dim は対象外。
