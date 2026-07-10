---
number: 163
title: feat(daemon): IPC + attach プロトコルの土台（Step 3a）
status: done
priority: medium
labels: [daemon, ipc]
dependson: []
related: []
parent: 159
created_at: 2026-07-10T03:24:36.198899+00:00
updated_at: 2026-07-10T03:24:40.165892+00:00
---

Epic #159 の Step 3（PTY 所有の移設）の第一スライス。PTY 実体の移設に先立ち、**daemon の client/server IPC の土台**を実装する。

## 実装内容

- daemon が Unix domain socket（`<data-dir>/daemon/sock`）を開き、クライアントが接続できる。
- プロトコル: `ClientMessage`（`list_sessions` / `subscribe` / `unsubscribe`）と `ServerMessage`（`sessions` / `error`）、長さ前置（u32 BE）フレーム。
- `subscribe` した接続には、監視ティックがスナップショット変化を検知するたびに `Sessions` を push（daemon→client の live 配信）。`list_sessions` は一発取得。
- 次スライス（3b）で配信内容を `Sessions` から `Screen`（PTY 画面 vt100）へ置き換える土台。

## 層構成

- `domain/daemon_ipc.rs` — メッセージ型 + 長さ前置フレームコーデック（`FrameDecoder`、上限ガード付き）。純粋。
- `infrastructure/daemon_ipc.rs` — socket パス解決 + メッセージ JSON エンコード/デコード。
- `usecase/daemon_ipc.rs` — `SubscriberRegistry` + `handle`（メッセージ dispatch）。純粋。
- `src/main.rs`（合成ルート・除外）— `DaemonIpcServer`（単一スレッドの非ブロッキング accept/read/dispatch/write イベントループ）を serve ループに組み込み、監視変化時に購読者へ push。

## テスト

- domain/usecase/infra の全分岐をユニットテストでカバー（カバレッジ 100%: lines/functions）。フレーム分割・上限超過・不正 JSON・購読レジストリ・dispatch を網羅。
- `tests/daemon_ipc_test.rs` — 実 `usagi daemon` を起動し UnixStream で接続、`list_sessions` → framed `Sessions` 応答を検証する e2e。

## スコープ外（後続）

- 3b: `TerminalPool` の daemon への移設・`Attach`/`Screen`/`Keys`/`Resize`・vt100 権威。
- Step 4: 通知調停・マルチクライアント入力調停・孤児 adopt。
- Windows（named pipe）対応。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 3a。
