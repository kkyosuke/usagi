---
number: 536
title: test(daemon): 実 PTY reattach 一致の checkpoint E2E と proposal 畳み込み
status: todo
priority: medium
labels: [test, v2, daemon, tui, terminal, e2e]
dependson: [535]
related: [524]
parent: 524
created_at: 2026-07-24T12:47:33.158614+00:00
updated_at: 2026-07-24T12:47:33.158614+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 5**。#535 の上に構築する（最終フェーズ）。

## 目的

実 daemon + 実 PTY の end-to-end で checkpoint restore の一致を検証し、設計を正本ドキュメントへ畳み込む。

## 対象責務

- 実 daemon + 実 PTY + fresh client/TUI E2E（`crates/daemon/tests/` の `agent_real_pty.rs` 隣接の新 target）で 64 KiB 超の unique output・long-running SGR・alternate screen・cursor save/restore・primary scrollback/copy marker を生成。
- client disconnect → reattach/resync 後に **child PID / spawn count 不変**、visible cells/cursor/style・primary saved buffer・`cells_with_scrollback`/copy history が before/reference と一致することを assert（設計テスト #1、受入条件 8）。
- Agent/generic・resize・resync・exit final snapshot で同一 contract を使うことを共通 fixture で確認。
- 設計を正本へ畳み込む: [12-terminal-vt-snapshot.md](../../document/proposals/12-terminal-vt-snapshot.md) を README 一覧で「畳み込み済み」に落とし、[04-ipc.md](../../document/04-ipc.md) / [03-tui.md](../../document/03-tui.md) の最終整合を確認。

## 受入条件

- [ ] reattach 前後で child PID / spawn count 不変、全 buffer / copy history 一致を実 PTY で assert。
- [ ] 実 IPC frame size / allocation peak を assert。
- [ ] proposal 12 を畳み込み済みに更新。
- [ ] coverage 100% / clippy / fmt / Markdown link check 緑。

この Phase 完了時に #524 を `done` にして PR に載せる。
