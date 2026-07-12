---
number: 246
title: test(tui): parity real PTY regression を追加する
status: todo
priority: high
labels: [tui, test, pty]
dependson: [235, 238, 237]
related: []
parent: 227
created_at: 2026-07-12T21:12:34.175125+00:00
updated_at: 2026-07-12T21:12:34.175125+00:00
---

## 目的

実端末境界で entry/restore、input passthrough、resize、detach/reattach、quit の回帰を固定する。

## スコープ

- alternate screen/raw mode/cursor/mouse 復元、reserved/non-reserved input、resize artifact、PTY reattach。

## 対象外

- daemon PTY broker/crash continuation、visual golden の再実装。

## Acceptance ID

- release quality: real PTY regression。

## 依存

- #235/#237/#238。

## 検証

- deterministic real PTY integration test を追加し CI で実行する。
