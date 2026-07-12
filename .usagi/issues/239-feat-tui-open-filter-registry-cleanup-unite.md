---
number: 239
title: feat(tui): Open filter・registry cleanup・Unite を追加する
status: done
priority: medium
labels: [tui, parity-b]
dependson: [230]
related: []
parent: 227
created_at: 2026-07-12T21:12:33.292868+00:00
updated_at: 2026-07-12T22:54:30.606703+00:00
---

## 目的

Open surface に workspace filter、欠損 registry cleanup、Unite selection を追加する。

## スコープ

- filter、Single/Unite 選択、欠損登録の確認付き cleanup、Unite set の open。

## 対象外

- Open の着地 animation、複数 workspace sidebar、registry backend の所有権変更。

## Acceptance ID

- `B-OPEN-1`（proposal の後回し項目: Open filter/cleanup/Unite）。

## 依存

- #230 の Open entry state。registry port は core 所有のまま利用する。

## 検証

- fake registry で filter、cleanup confirm/cancel、Single/Unite selection scenario。
