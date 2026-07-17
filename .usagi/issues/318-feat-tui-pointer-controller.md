---
number: 318
title: feat(tui): pointer 入力を controller 語彙へ移す
status: todo
priority: medium
labels: [tui, controller, input]
dependson: [315]
related: [258]
created_at: 2026-07-17T14:23:02.083365+00:00
updated_at: 2026-07-17T14:23:02.083365+00:00
---

## 目的

#315 の暫定 seam（shell が hit-test して `AppKey::SelectRow(Selection)` に翻訳）を恒久化する。`AppEvent::Pointer` を controller に導入し、sidebar クリックの解釈を shell から reducer へ移す。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.2 / §8-2。

## スコープ

- `AppEvent::Pointer`（座標＋種別）を追加し、`update()` が `HomeProjection::row_at` と同じ viewport 計算で行を解決して選択・活性化する。
- shell 側の hit-test / `AppKey::SelectRow` 変換を撤去する。
- terminal pane 内の drag / copy は Home 行契約と無関係のため対象外（shell + `TerminalSession` のまま）。

## 完了条件

- sidebar のクリック選択が reducer テストで固定される（viewport 先頭・末尾 wrap・`+ new session` 行を含む）。
- coverage 100% を維持する。
