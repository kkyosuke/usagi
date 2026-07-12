---
number: 220
title: feat(clients): TUI／CLI／MCP を v2 daemon IPC へ cutover する
status: done
priority: high
labels: [tui, cli, ipc]
dependson: [209, 219]
related: []
parent: 213
created_at: 2026-07-12T11:40:00.006399+00:00
updated_at: 2026-07-12T21:22:53.471170+00:00
---

## 目的

TUI、CLI、MCP の managed session／terminal 経路を共通 v2 IPC client portへ移し、daemon authorityとdisconnect/reconnect契約を実利用面で完成させる。設計は [clean architecture 配置](../../document/proposals/05-daemon-lifecycle.md#clean-architecture-上の配置) を正本とする。

## 対象

- TUI: terminal snapshot/output state machine、tab/focus/layoutだけをローカル所有し、generation別attach/resume/resyncを行う。
- CLI/MCP: session command/toolをdaemon requestへ変換し、accepted operation／progress／structured errorを人間向け／JSON-RPC向けに整形する。
- coreのsurface-neutral client port、合成ルートのUnix connect/autospawn、clientごとのtimeout/retry policy。
- open pane snapshotは`TerminalRef`／`AgentRuntimeId?`を保存し、path/name/u64 terminal idを復旧keyにしない。
- managed daemonが不在／incompatible／ownership unknownのときローカルPTYへ暗黙fallbackせず、autospawnまたはtyped errorにする。

## 受け入れ条件

- TUI終了後もAgentが継続し、再起動でsnapshotまたはresumeから同じterminalへ復帰する。
- 複数TUI、CLI、MCPが同時接続してもcorrelation／subscription／resize／inputが混線しない。
- session create/remove/setup/promptがTUI不在でも進み、TUIはdaemon pushだけで状態を更新する。
- stale pane、same-name recreation、generation rollover、daemon crash orphanを安全表示し、勝手にreplacement spawnしない。
- current v2 direct session mutation／local managed PTY経路を削除または非managed recovery専用へ隔離する。
- 実socket + PTYのblack-box E2Eでdisconnect耐性、multi-client、operation reconcile、no-fallback、rolloverを検証する。
