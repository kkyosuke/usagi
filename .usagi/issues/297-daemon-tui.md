---
number: 297
title: Daemon-TUI メトリクスサブスクリプションの実装: 登録/解除/再接続/複数クライアント/遅いクライアント/スキーマ
status: done
priority: high
labels: []
dependson: []
related: [295]
created_at: 2026-07-13T22:24:07.582060+00:00
updated_at: 2026-07-13T22:28:30.893159+00:00
---

## 目的

TUI 起動時に daemon の metrics subscriber を登録し、daemon が定期的な metrics を push する v2 IPC を実装する。#295 の Agent launch effect/runtime completion とは責務を混ぜない。

## 受け入れ条件

- 共有 IPC schema に subscribe / unsubscribe / metrics event を定義し、versioned な値として decode/encode する。
- TUI composition は起動時に登録し、終了時に解除する。接続断後は安全に再接続・再登録する。
- daemon は複数 subscriber を独立して管理し、定期 snapshot を通知する。
- subscriber の切断は daemon 状態や他 client を止めない。遅い client は bounded queue / coalescing により metrics を滞留させず、必要時に切断する。
- schema、登録解除、再接続、複数 TUI、遅い client、周期通知を unit / IPC integration test で検証する。
- v2 document を更新し、品質 gate を通過して PR にする。
