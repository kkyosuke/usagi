---
number: 164
title: feat(daemon): daemon が PTY を所有する（Step 3b-1）
status: done
priority: medium
labels: [daemon, ipc, pty]
dependson: []
related: []
parent: 159
created_at: 2026-07-10T12:32:08.221044+00:00
updated_at: 2026-07-10T12:32:13.551021+00:00
---

Epic #159 の Step 3b（PTY 所有の移設）の第一スライス。**daemon が worktree ごとの端末を自プロセスの子として所有**し、要求クライアントの切断後も端末が生き続けるようにする。単一端末で「TUI（クライアント）を閉じても agent が走り続ける」が初めて成立する。

## 実装内容

- IPC に `Spawn { worktree }` / `Kill { worktree }`（`ClientMessage`）と `Spawned { worktree, pid }` / `Killed { worktree }`（`ServerMessage`）を追加。
- daemon が `PtySession`（既存の portable-pty 基盤）を worktree キーで所有。`Spawn` は端末を起こし（既存なら再利用）、pid を返す。`Kill` は該当 `PtySession` を drop してプロセスグループを SIGKILL。
- クライアント切断時に落とすのは socket だけで、端末の `PtySession` は daemon が保持し続ける → **プロセスが生存**。
- `daemon stop` 時は全端末を kill（孤児シェルの leak 防止）。

## 層構成

- `domain/daemon_ipc.rs` — `Spawn`/`Kill`/`Spawned`/`Killed` メッセージ追加。
- `usecase/daemon_ipc.rs` — `handle` を `Action`（`Reply`/`Spawn`/`Kill`/`Nothing`）を返す形に変更（PTY IO は合成ルートが担うため）。`TerminalRegistry`（worktree→pid の純粋台帳）追加。
- `src/main.rs`（合成ルート・除外）— `DaemonIpcServer` が `PtySession` を worktree キーで保持、`spawn_terminal`/`kill_terminal`、shutdown で全 kill。

## テスト

- domain/usecase の全分岐をユニットテスト（カバレッジ 100%: lines/functions）。`Action` 分岐・`TerminalRegistry` の insert/pid/remove/replace を網羅。
- `tests/daemon_ipc_test.rs` に e2e 追加: 実 daemon で `spawn` → クライアント切断 → **pid が生存していることを検証** → 別クライアントで `kill` → プロセス消滅を検証。

## スコープ外（後続）

- **3b-2**: `Screen` ストリーミング（daemon 側 vt100 権威、購読者へ画面差分 push）。
- **3b-3**: TUI を attach クライアント化（`Keys`/`Resize`、`TerminalPool` 置換）。
- Step 4: 通知調停・マルチクライアント入力調停・孤児 adopt。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 3b-1。
