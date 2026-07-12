---
number: 235
title: feat(tui): daemon terminal inventory/stream と pane reattach を結合する
status: done
priority: high
labels: [tui, ipc, pane]
dependson: [218, 220, 232]
related: []
parent: 227
created_at: 2026-07-12T21:11:47.528328+00:00
updated_at: 2026-07-12T22:58:10.052712+00:00
---

## 目的

D1/D3/D4/D6 の terminal inventory、attach/resume、input/resize、local resume state を pane reducer に結合する。

## スコープ

- `TerminalRef` の保存/検証、pending→live、exit、attach/resync、geometry dedupe。
- target/tab 選択の attach policy と disconnect/orphan safe state。

## 対象外

- daemon terminal registry/PTY 実装、pure tab model の再実装。

## Acceptance ID

- `A-PANE-1` の daemon/PTY integration。

## 依存

- #218/#220 と #232。

## 検証

- fake daemon + real PTY integration で detach/reattach/exit/resync/resize を確認する。
