---
number: 533
title: feat(core): ScreenCheckpoint 型と bounded/hostile decode
status: todo
priority: high
labels: [feat, v2, core, terminal, vt, correctness]
dependson: [532]
related: [524]
parent: 524
created_at: 2026-07-24T12:46:27.738605+00:00
updated_at: 2026-07-24T12:46:27.738605+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 2**。#532 の上に構築する。

## 目的

`usagi-core` の `VtScreen` に versioned semantic checkpoint の serialize/deserialize を追加する。serialize 責務を parser authority と同居させ、daemon/TUI で二重実装しない。

## 対象責務

- `ScreenCheckpoint` / `BufferCheckpoint` / `RowCheckpoint`(run-length) / `CellRun` / `DecoderCheckpoint` を定義（serde derive）。primary/alternate/**saved primary** buffer・cursor/saved cursor・scroll region・scrollback・interned attribute table(`styles`)・decoder(phase/params/utf8_pending) を表す。
- `VtScreen::checkpoint()` と `VtScreen::from_checkpoint()` を実装。
- **decode は 算術検証 → 予算検証 → 確保 の順**。`ROWS_MAX`/`COLS_MAX`/`CELLS_PER_TERMINAL_MAX`(`checked_mul`)/`SCROLLBACK_MAX`/`STYLES_MAX`/`PARAMS_MAX`/`UTF8_PENDING_MAX`/`CHECKPOINT_BYTES_MAX` と `style_id < styles.len()` を検証。範囲外・overflow・未知 `schema_version` は typed error で fail closed（panic / unbounded allocation / blank parser corruption を起こさない）。

## 受入条件

- [ ] round-trip: 任意の `VtScreen` state → `checkpoint()` → `from_checkpoint()` が元 state と一致（property）。
- [ ] 64 KiB 超出力で UTF-8/CSI/OSC/SGR/alt/combining/CJK/malformed の**全 split 位置**を、untrimmed reference parser と checkpoint 復元後 state で property/fixture 比較（設計テスト #2）。
- [ ] rows/cols 0・最大値・乗算 overflow・巨大 cell/attribute/scrollback count・compression bomb 相当を fuzz/property し、確保前 bounded rejection を測定（設計テスト #4）。
- [ ] coverage 100% / clippy / fmt 緑。

## 非目標

- wire negotiation・daemon 生成・TUI reconstruct は後続 Phase。
