---
number: 264
title: feat(daemon): generic terminal IPC runtime を接続する
status: todo
priority: high
labels: [daemon, ipc, terminal]
dependson: [255]
related: [235]
parent: 227
created_at: 2026-07-13T00:15:59.432663+00:00
updated_at: 2026-07-13T00:16:20.632381+00:00
---

## 目的

daemon-owned generic shell terminal の launch / attach / stream を、現在の pure coordinator から実 IPC server の唯一の terminal ownership loop へ接続する。client supplied command / argv / env は受け付けず、TUI・CLI・MCP は daemon client port だけを利用する。

## 現状の根拠

- `crates/daemon/src/usecase/generic_terminal.rs` は durable reservation、snapshot、detach/reconcile の pure coordinator を持つ。
- `crates/core/src/usecase/client.rs` の `TerminalAction` は attach/resume/resync/input/resize のみで launch がない。
- `crates/daemon/src/presentation/ipc.rs::dispatch` は terminal request を所有 loop へ渡さず、body を response に echo する。
- 合成 root の IPC server は handshake adapter のみを thread に接続しており、PTY/runtime を生成しない。

## スコープ

- product-neutral な terminal launch intent（stable profile ID、fully fenced workspace/session/worktree scope、geometry）を IPC request/response vocabulary に追加する。raw command、argv、environment、secret は wire に追加しない。
- daemon の connection handler を、generic terminal coordinator / terminal registry / trusted profile resolver / injected PTY process adapter を持つ daemon-owned runtime へ接続する。
- launch、inventory、attach/resume/resync、input、resize、detach、output/exit event を `TerminalRef`、cursor、connection subscription、request/input sequence で fence する。
- disconnect が attachment だけを外し PTY/process を残すこと、stale/gap/orphan/reconnect が local replacement spawn を起こさないことを保証する。
- 合成 root で実 daemon runtime を開始し、secure Unix transport 上の client request をこの ownership loop へ渡す。

## 対象外

- TUI の Closeup UX、renderer、キー dispatch（後続 issue）。
- Agent adapter launch、session create、local fallback。
- client supplied program/argv/env、PTY master FD の daemon crash 越し継続。

## 受け入れ条件

- daemon client が session/root scope と safe profile ID だけで generic terminal launch を要求でき、response は完全な `TerminalRef` を返す。
- attach は atomic snapshot + subscription を返し、stream event は output offset の連続性を示す。input / resize / detach は exact `TerminalRef` と connection ownership を検証する。
- unavailable、stale target、ownership unknown、partial/ambiguous write は typed safe error となり、server/TUI が local PTY を spawn しない。
- fake IPC client と injected fake PTY による e2e で launch → attach → output → input → detach → reattach → exit を確認する。
- 実装済みの daemon contract を `document/04-ipc.md` と `document/05-daemon.md` の正本へ反映する。
