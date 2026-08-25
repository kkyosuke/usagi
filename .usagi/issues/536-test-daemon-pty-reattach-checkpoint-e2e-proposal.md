---
number: 536
title: test(daemon): 実 PTY reattach 一致の checkpoint E2E と proposal 畳み込み
status: done
priority: medium
labels: [test, v2, daemon, tui, terminal, e2e]
dependson: [535]
related: [524]
parent: 524
created_at: 2026-07-24T12:47:33.158614+00:00
updated_at: 2026-07-25T02:06:51.185153+00:00
---

[#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md) の設計 [`document/proposals/12-terminal-vt-snapshot.md`](../../document/proposals/12-terminal-vt-snapshot.md) の **Phase 5**。#535 の上に構築する（最終フェーズ）。

## 目的

実 daemon + 実 PTY の end-to-end で checkpoint restore の一致を検証し、設計を正本ドキュメントへ畳み込む。

## 対象責務

- 実 daemon + 実 PTY + fresh client/TUI E2E（`crates/daemon/tests/` の `agent_real_pty.rs` 隣接の新 target）で 64 KiB 超の unique output・long-running SGR・alternate screen・cursor save/restore・primary scrollback/copy marker を生成。
- Agent/generic・resize・resync・exit final snapshot で同一 contract を使うことを共通 fixture で確認。
- 設計を正本へ畳み込む: [12-terminal-vt-snapshot.md](../../document/proposals/12-terminal-vt-snapshot.md) を README 一覧で「畳み込み済み」に落とし、[04-ipc.md](../../document/04-ipc.md) / [03-tui.md](../../document/03-tui.md) の最終整合を確認。

## 受入条件

- [x] reattach 前後で child PID / spawn count 不変、全 buffer / copy history 一致を実 PTY で assert。
- [x] 実 IPC frame size / allocation peak を assert。
- [x] proposal 12 を畳み込み済みに更新。
- [x] coverage 100% / clippy / fmt / Markdown link check 緑。

この Phase 完了時に #524 を `done` にして PR に載せる。

## 実装メモ

E2E は `crates/daemon/tests/terminal_checkpoint_real_pty.rs`（1 test / 2 scenario）。共通 fixture を
`DaemonOwner` trait 越しに Agent owner（`AgentRuntime`）と generic owner（`GenericTerminalRuntime`）の
両方へ適用し、実 PTY が 100 KiB 超（64 KiB journal 超）を描いたあと attach → disconnect → fresh client
reattach → resync → resize → 実 input による exit → final snapshot まで 1 本で assert する。比較基準は
**全 byte を見た untrimmed reference parser** で、`VtScreen` の完全一致（可視 grid・scrollback・cursor・
saved cursor・scroll region・SGR・alternate と saved primary buffer・decoder 状態）を要求する。
legacy raw tail は同じ run 内で counter-example として測り、reference を再現できず window 以前の履歴を
失っていることを assert する。

この E2E が **#534 由来の性能欠陥**を検出した: `RuntimeCoordinator::append_output` が offset を読むためだけに
`TerminalRegistry::snapshot`（= 完全な checkpoint 生成 + JSON size 計測）を呼んでいたため、Agent の PTY chunk
ごとに screen 全体を serialize していた（本 E2E で 100 KiB の描画に 43 秒）。offset だけを返す
`TerminalRegistry::output_window` を追加し、append_output と Agent/generic 両方の `completed_inventory`
（tombstone ごとに checkpoint を生成していた）を切り替えて 2.2 秒になった。
