---
number: 165
title: feat(daemon): 端末画面を IPC でストリーミングする（Step 3b-2）
status: done
priority: medium
labels: [daemon, ipc, pty]
dependson: []
related: []
parent: 159
created_at: 2026-07-10T12:43:51.319300+00:00
updated_at: 2026-07-10T12:43:56.908403+00:00
---

Epic #159 の Step 3b-2。daemon が所有する端末の vt100 画面を、attach したクライアントへ IPC でストリーミングする。3b-1（daemon が PTY を所有）に依存。

## 実装内容

- IPC に `Attach { worktree }` / `Detach { worktree }`（`ClientMessage`）と `Screen { worktree, contents }`（`ServerMessage`）を追加。`contents` は vt100 `contents_formatted()` の replay バイト列。
- `attach` 時に現在画面を即送信。以降、serve ループ毎ティックで各端末の画面世代（`PtySession::generation()`）の変化を検知し、attach クライアントへ `Screen` を push（未 attach の端末は世代追跡もしない）。
- クライアント切断時は購読・attach の両方から除去。

## 層構成

- `domain/daemon_ipc.rs` — `Attach`/`Detach`/`Screen` メッセージ追加。
- `usecase/daemon_ipc.rs` — `AttachTable`（worktree→client 集合の純粋台帳）追加、`handle` に `Attach`/`Detach` → `Action::Attach`/`Detach`。
- `src/main.rs`（合成ルート・除外）— `DaemonIpcServer` に attach テーブル・画面世代マップ、`stream_screens`（変化検知 push）、attach 時の即時送信、`current_screen`（vt100 → bytes）。

## テスト

- domain/usecase の全分岐をユニットテスト（カバレッジ 100%: lines/functions）。`AttachTable` の attach/detach/remove_client/clients_for、`Action` 分岐を網羅。
- `tests/daemon_ipc_test.rs` に e2e 追加: 実 daemon で `spawn` → `attach` → 該当 worktree の `Screen` メッセージ受信を検証（vt100 画面が IPC で届くことを実証）。

## スコープ外（後続）

- **3b-3**: `Keys`/`Resize` と TUI の attach クライアント化（`TerminalPool` 置換）。
- Step 4 / Step 5。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 3b-2。
