---
number: 242
title: feat(tui): Overview の completion/history/help と top-level UX を追加する
status: done
priority: medium
labels: [tui, parity-b]
dependson: [226, 230]
related: []
parent: 227
created_at: 2026-07-12T21:12:33.836768+00:00
updated_at: 2026-07-12T22:55:39.529965+00:00
---

## 目的

Overview registry を基点に Tab completion、history recall、command help/long text と top-level shortcut の発見性を追加する。

## スコープ

- registry metadata に基づく completion/history/help、結果帯の操作。
- Welcome/Open/Home の keyboard-first top-level UX を整える。

## 対象外

- command registry/effect dispatch の再実装、mouse/Alt scheme、sidebar toggle。

## Acceptance ID

- `B-OVERVIEW-1`（proposal の Overview 後回し項目）。

## 依存

- #226、#230。

## 検証

- registry fake と modal render で completion/history/help/long text を確認する。
