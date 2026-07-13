---
number: 270
title: fix(daemon): root IPC server に generic terminal owner を接続する
status: done
priority: high
labels: [daemon, ipc, terminal, runtime]
dependson: [264]
related: [263, 268]
parent: 227
created_at: 2026-07-13T01:32:29.174167+00:00
updated_at: 2026-07-13T01:37:41.374955+00:00
---

## 背景

#264 の coordinator・IPC vocabulary・fake PTY 契約は実装済みだが、`src/runtime/daemon.rs` の root IPC server は `SessionRuntime` だけを共有し、`GenericTerminalRuntime` / `TerminalOwner`、trusted profile resolver、durable terminal store、実 PTY の output pump を組み立てていない。このため Unix socket 越しの generic terminal は実 PTY を所有・継続できない。

## スコープ

- root IPC server で generic terminal owner を一つ生成し、全 connection が共有する。
- session request の既存 routing を保ったまま terminal request を owner へ渡す。
- trusted `login-shell` profile、durable terminal record、実 PTY の spawn/input/output を composition adapter に接続する。
- detach/disconnect は subscription だけを外し、再接続時は同じ owner の snapshot/journal へ attach する。

## 対象外

- #268 の session lifecycle runtime の実装・変更。
- #263 の agent launch / Closeup UX の変更。
- daemon crash をまたぐ PTY master FD の継続。

## 受け入れ条件

- Unix IPC で launch → attach → input → PTY output → detach → reconnect が実 PTY に対して動く。
- root IPC server が terminal owner を connection 間で共有し、session request routing を回帰させない。
- trusted profile 以外、stale reference、接続切断は安全に拒否または detach-only となり、client 側 fallback spawn を起こさない。
- 実装済み contract は daemon / IPC ドキュメントに反映される。
