---
number: 229
title: feat(tui): crossterm event pump を統一入力 stream へ変換する
status: done
priority: high
labels: [tui, runtime, input]
dependson: [224]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.138830+00:00
updated_at: 2026-07-12T22:33:09.385020+00:00
---

## 目的

実端末の crossterm event を lossless な TUI input 語彙へ変換し、key・resize・tick・backend event を controller の単一 stream に合流させる。

## スコープ

- key kind/modifier/text/paste/resize を保持する adapter と poll/event pump。
- key、resize、tick、backend receiver を一つの runtime event stream に多重化する seam。

## 対象外

- #224 の pure encoder/classifier の再実装、daemon IPC client、frame diff renderer。

## Acceptance ID

- `A-INPUT-1` / `A-INPUT-2` の実端末入力への接続部分。

## 依存

- #224。renderer/runtime 合成は #241、daemon backend は #220 後の各 adapter issue が担当する。

## 検証

- fake crossterm source の sequence test、key/resize/tick/backend ordering test。
