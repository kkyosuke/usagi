---
number: 228
title: feat(tui): frame-diff renderer と Unicode 幅・resize reset を実装する
status: done
priority: high
labels: [tui, renderer]
dependson: [225]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.068513+00:00
updated_at: 2026-07-12T22:33:34.851266+00:00
---

## 目的

描画 model から pure frame diff を生成し、実端末 write を adapter に閉じ込める。

## スコープ

- frame/cell grid、row/column/span diff、短縮時の stale suffix clear、surface reset と resize full clear。
- ANSI 幅 0、CJK wide glyph 幅 2、Ambiguous 幅 1、wide glyph 非分断の fixture。

## 対象外

- crossterm の event polling、daemon PTY resize 送信、既存 acceptance の再定義。

## Acceptance ID

- `A-RENDER-1`（pure renderer の範囲）。

## 依存

- #225 の Home projection を frame 入力として使う。実端末結合は #240/#241 で行う。

## 検証

- pure renderer unit test と golden frame test。
