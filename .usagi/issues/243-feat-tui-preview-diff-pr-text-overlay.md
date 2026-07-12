---
number: 243
title: feat(tui): preview・diff・PR・text overlay を実装する
status: todo
priority: medium
labels: [tui, parity-b, overlay]
dependson: [226, 228]
related: []
parent: 227
created_at: 2026-07-12T21:12:33.941269+00:00
updated_at: 2026-07-12T21:12:33.941269+00:00
---

## 目的

Home 背景を保ったまま preview、diff、PR、長文 text overlay を安全に表示・scroll できるようにする。

## スコープ

- overlay state、scroll/clip、diff/PR/text data port と safe fallback。

## 対象外

- rich syntax theme、backend の diff/PR fetch 実装、note/env editor。

## Acceptance ID

- `B-OVERLAY-1`（preview/diff/PR/text の範囲）。

## 依存

- #226 の overlay dispatch と #228 renderer。

## 検証

- reducer/render golden で背景保持、tiny size、long text scroll を確認する。
