---
number: 166
title: feat(daemon): daemon 端末への入力（Keys/Resize）（Step 3b-3）
status: done
priority: medium
labels: [daemon, ipc, pty]
dependson: []
related: []
parent: 159
created_at: 2026-07-10T13:23:51.644405+00:00
updated_at: 2026-07-10T13:23:58.272304+00:00
---

Epic #159 の Step 3b-3。daemon 所有端末へ IPC 越しに入力・リサイズできるようにする。3b-2（Screen ストリーミング）に依存。これで daemon 端末の I/O（spawn→attach→入力→画面出力）が IPC で完結する。

## 実装内容

- IPC に `Keys { worktree, data }`（入力バイト列）/ `Resize { worktree, cols, rows }`（`ClientMessage`）を追加。
- daemon は該当 worktree の `PtySession` へ `write(data)` / `resize(rows, cols)`。応答はなく、結果は既存の `Screen` push で attach クライアントへ流れる。

## 層構成

- `domain/daemon_ipc.rs` — `Keys`/`Resize` メッセージ。
- `usecase/daemon_ipc.rs` — `Action::Keys`/`Resize`、`handle` の対応分岐。
- `src/main.rs`（合成ルート・除外）— `write_terminal`/`resize_terminal`。

## テスト

- domain/usecase の全分岐をユニットテスト（カバレッジ 100%: lines/functions）。
- `tests/daemon_ipc_test.rs` に e2e 追加: `spawn` → `attach` → `Keys`（`printf usagi-keys-ok\n`）→ 画面に marker が現れることを検証（**入力→端末→Screen 出力の往復を実証**）。

## スコープ外（後続 = 3b-4）

TUI の `TerminalPool` を daemon 所有端末への attach に置き換える大改造（`pool.rs`/`pane.rs`/`home/mod.rs`）。カバレッジ除外の TUI 内部を触り、実端末での手動検証（`run`/`verify`）が必須のため専用スライスで進める。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 3b-3。
