---
number: 535
title: fix(tui): checkpoint negotiation と screen reconstruct（legacy fail-closed）
status: todo
priority: high
labels: [fix, v2, tui, terminal, vt, replay, correctness]
dependson: [534]
related: [524, 199]
parent: 524
created_at: 2026-07-24T12:47:15.237736+00:00
updated_at: 2026-07-24T12:47:15.237736+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 4**。#534 の上に構築する。この Phase で P1 correctness が解消する。

## 目的

TUI が attach/resync で **blank parser に raw tail を流す**のをやめ、daemon の semantic checkpoint から screen を復元する。

## 対象責務

- `TerminalAttach` を revision 2 の checkpoint 版へ拡張し、`TerminalSession::replace`（`crates/tui/src/usecase/application/terminal_session.rs`）を `core::VtScreen::from_checkpoint(...)` + `Resume` suffix feed へ置き換える。`screen_for(geometry).advance(&replay)` を撤去。
- capability(`terminal.screen-checkpoint.v1`)/revision(gen1 rev2) negotiation。共通 revision が 1 に落ちる、または capability 不在の場合は **legacy raw tail を parser へ流さず**、安全な限定表示（履歴復元不可の typed state）へ fail closed し、`output_offset` 以降の live 出力のみ描画。
- geometry/revision fence を復元後に検証し、mismatch は snapshot retry / typed resync（state 混在なし）。

## 受入条件

- [ ] retention 先頭が UTF-8/CSI/OSC/SGR/alt 途中でも reconnect 前後の visible cells/cursor/style が一致（設計受入条件 1/3）。
- [ ] primary/alternate/saved primary buffer・`cells_with_scrollback`・selection/copy history が untrimmed reference と一致（受入条件 2）。
- [ ] old/new client × old/new daemon × capability present/absent × supported/unknown revision の compatibility matrix を固定し、途中 escape を legacy raw parser へ渡さないことを assert（設計テスト #3）。
- [ ] resize interleave 時の TUI 側 fence（設計テスト #5、TUI 側）。
- [ ] 実装に合わせ [03-tui.md#live-terminal-の出力表示と入力](../../document/03-tui.md#live-terminal-の出力表示と入力) を更新（visible + primary/copy-history restore、legacy 限定表示）。
- [ ] coverage 100% / clippy / fmt 緑。
