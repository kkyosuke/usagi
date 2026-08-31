---
number: 534
title: feat(daemon): terminal grid authority と revision 2 checkpoint snapshot
status: done
priority: high
labels: [feat, v2, daemon, terminal, vt, replay]
dependson: [533]
related: [524, 472]
parent: 524
created_at: 2026-07-24T12:46:51.706653+00:00
updated_at: 2026-07-24T20:53:35.046993+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 3**。#533 の上に構築する。

## 目的

## 対象責務

- `crates/daemon/src/usecase/terminal.rs` の `Entry` に per-terminal `core::VtScreen` を持たせ、`append_output` で受信 byte を feed、`resize` で screen を resize する。
- wire generation 1 の `max_revision` を 1→2 に上げる。revision 2 の `Snapshot` は `replay: Vec<u8>` の代わりに `screen: ScreenCheckpoint` を持ち、`base_offset == output_offset`（tail 長 0）。
- daemon は `ServerHello.capabilities` に `terminal.screen-checkpoint.v1` を広告。
- checkpoint 生成時に `CHECKPOINT_BYTES_MAX` と process-local aggregate cell/scrollback budget を強制し、超過分は古い scrollback から bounded trim（trim 計上 counter を追加）。既定 1 MiB frame を超えない。
- resize の terminal actor 排他区間・revision fence を維持（#472 の bounded journal / #473 の FD 契約は再実装しない）。
- revision 1（raw）client には従来どおり raw tail を返す（移行互換）。

## 受入条件

- [ ] 実装に合わせ [04-ipc.md#generic-terminal-request](../../document/04-ipc.md#generic-terminal-request) を更新（snapshot schema / capability / revision / offset）。
- [ ] coverage 100% / clippy / fmt 緑。
