---
number: 241
title: feat(tui): Config の scope read/edit/save を settings port に結合する
status: in-progress
priority: medium
labels: [tui, parity-b]
dependson: [230]
related: []
parent: 227
created_at: 2026-07-12T21:12:33.756332+00:00
updated_at: 2026-07-12T22:48:45.483666+00:00
---

## 目的

Config の global/workspace scope を read-edit-save 可能にし、save failure でも編集値を保つ。

## スコープ

- scope 選択、dirty state、明示 Save、safe error/retry。

## 対象外

- Local LLM install/probe、settings backend の実装。

## Acceptance ID

- `B-CONFIG-1`。

## 依存

- #230 の entry/runtime seam。settings port は core/other backend 所有。

## 検証

- fake settings port で scope isolation、save failure/form retention を確認する。
