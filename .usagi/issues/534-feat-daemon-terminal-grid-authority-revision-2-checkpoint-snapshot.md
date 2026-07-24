---
number: 534
title: feat(daemon): terminal grid authority と revision 2 checkpoint snapshot
status: todo
priority: high
labels: [feat, v2, daemon, terminal, vt, replay]
dependson: [533]
related: [524, 199, 472]
parent: 524
created_at: 2026-07-24T12:46:51.706653+00:00
updated_at: 2026-07-24T12:46:51.706653+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 3**。#533 の上に構築する。

## 目的

daemon を terminal grid/scrollback の唯一の権威に戻す（#199 の shipping regression 復旧）。attach/resync snapshot を raw tail から semantic checkpoint へ置き換える。

## 対象責務

- `crates/daemon/src/usecase/terminal.rs` の `Entry` に per-terminal `core::VtScreen` を持たせ、`append_output` で受信 byte を feed、`resize` で screen を resize する。
- wire generation 1 の `max_revision` を 1→2 に上げる。revision 2 の `Snapshot` は `replay: Vec<u8>` の代わりに `screen: ScreenCheckpoint` を持ち、`base_offset == output_offset`（tail 長 0）。
- daemon は `ServerHello.capabilities` に `terminal.screen-checkpoint.v1` を広告。
- checkpoint 生成時に `CHECKPOINT_BYTES_MAX` と process-local aggregate cell/scrollback budget を強制し、超過分は古い scrollback から bounded trim（trim 計上 counter を追加）。既定 1 MiB frame を超えない。
- resize の terminal actor 排他区間・revision fence を維持（#472 の bounded journal / #473 の FD 契約は再実装しない）。
- revision 1（raw）client には従来どおり raw tail を返す（移行互換）。

## 受入条件

- [ ] revision 2 で `Snapshot.screen` が現在 screen を完全表現。`write_json_frame` で frame が既定上限内（設計テスト #6）。
- [ ] resize を checkpoint 直前 / capture 中 / suffix 適用前後に interleave しても geometry/revision mismatch が retry/typed resync になり state 混在しない（設計テスト #5、daemon 側）。
- [ ] per-terminal / aggregate allocation peak を assert（設計テスト #6）。
- [ ] 実装に合わせ [04-ipc.md#generic-terminal-request](../../document/04-ipc.md#generic-terminal-request) を更新（snapshot schema / capability / revision / offset）。
- [ ] coverage 100% / clippy / fmt 緑。
