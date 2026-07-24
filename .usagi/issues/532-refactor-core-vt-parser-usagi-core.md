---
number: 532
title: refactor(core): VT parser を usagi-core へ抽出（挙動不変）
status: todo
priority: high
labels: [refactor, v2, core, tui, terminal, vt]
dependson: []
related: [524, 199]
parent: 524
created_at: 2026-07-24T12:46:06.279910+00:00
updated_at: 2026-07-24T12:46:06.279910+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 1**。

## 目的

VT parser authority を単一箇所にするため、TUI 専用の `TerminalScreen`（VT state + parser + resize）を `usagi-core` の usecase 層 pure 型 `VtScreen` へ移す。daemon と TUI が同一 parser を共有できる土台を作る。**挙動は不変**（pure refactor）。

## 対象責務

- `crates/tui/src/usecase/application/terminal_screen.rs` の grid / scrollback / cursor / SGR / scroll region / alternate + saved buffer / UTF-8 decoder と `advance` / `resize` を `usagi-core` へ移す。
- 描画（`rows_with_scrollback_and_cursor_selection` / link scan / selection / cursor marker）は presentation 語彙に依存するため **TUI に残し**、core が公開する read-only cell API（`ch` / interned もしくは raw style / continuation / cursor / scrollback 反復）の上に載せ替える。
- `usagi-core` の依存に `unicode-width` を追加（usecase 層のみ。domain の `chrono`/`serde`/`uuid` 規則は不変）。[06-conventions.md#依存クレート](../../document/06-conventions.md#依存クレート) の表を更新。
- 依存方向（`usagi-core` から実行面クレートを参照しない）を守る。

## 受入条件

- [ ] `TerminalScreen` の既存 unit test 相当が core 側で緑（挙動不変）。TUI 側は core screen を wrap した描画テストを維持。
- [ ] `cargo test --workspace` / clippy / fmt / coverage 100% が緑。
- [ ] `usagi-tui` の右ペイン描画・selection・link・cursor 表示が変わらない（既存 parity/presentation テストが通る）。

## 非目標

- checkpoint 型・serialize は Phase 2 で追加する（本 Phase は state model の移設のみ）。
- daemon / wire / TUI reconnect 契約は変更しない。
