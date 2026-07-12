---
number: 236
title: feat(tui): daemon phase と connection feedback を runtime に結合する
status: todo
priority: high
labels: [tui, ipc, feedback]
dependson: [219, 220, 233]
related: []
parent: 227
created_at: 2026-07-12T21:11:47.627+00:00
updated_at: 2026-07-12T21:11:47.627+00:00
---

## 目的

D2/D3/D5/D6 push を phase/feedback projection に配線し、キー無しの状態更新と再接続を実現する。

## スコープ

- phase、operation/terminal error、disconnect/reconnect/resync event の adapter。
- error envelope の safe message/error_id だけを UI 状態へ渡す。

## 対象外

- daemon event schema、純粋 projection/ranking の再実装。

## Acceptance ID

- `A-PHASE-1` / `A-FEEDBACK-1` の D2/D3/D5/D6 結合。

## 依存

- #219/#220 と #233。

## 検証

- fake IPC push/tick と reconnect scenario、safe error rendering test。
