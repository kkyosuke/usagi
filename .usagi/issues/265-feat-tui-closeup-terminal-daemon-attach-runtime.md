---
number: 265
title: feat(tui): Closeup terminal を daemon attach runtime へ接続する
status: todo
priority: high
labels: [tui, terminal, ipc]
dependson: [264, 282]
related: [235, 257, 279, 282]
parent: 227
created_at: 2026-07-13T00:16:40.138277+00:00
updated_at: 2026-07-13T11:20:30.723116+00:00
---

## 目的

親 usagi v2 TUI の Closeup / Overview command、effect、renderer、runtime effect runner を一つの daemon-owned terminal launch / attach UX に接続する。TUI は local PTY を spawn せず、daemon client の typed terminal request と stream だけを消費する。

## 現状の根拠

- `crates/tui/src/usecase/application/controller.rs` は `Effect::OpenTerminal` を生成するが、実 runner に接続されていない。
- `crates/tui/src/usecase/application/pane_runtime.rs` は `TerminalPort` を通じた attach/reconnect/input/resize と stream cursor 検証を実装済みである。
- 実 parent runtime は `crates/tui/src/presentation/mod.rs` の旧 `WorkspaceView` を駆動し、Closeup の Enter action を no-op としている。
- daemon terminal launch/stream の実 IPC ownership loop は #264 が提供する。

## スコープ

- Closeup の `terminal` action と command-palette input を同じ validated effect に正規化し、空引数と許可された `open` / `new` の UX を定義する。target は stable workspace/session identity を使用し、表示名・path で再探索しない。
- `Effect::OpenTerminal` を、daemon client による existing terminal attach または generic terminal launch → attach に接続する effect runner を導入する。daemon が返す完全な `TerminalRef` を pane reducer に保持する。
- `PaneRuntime` と actual terminal event pump / renderer を合成し、snapshot replay、output cursor、input、resize、exit、disconnect/reconnect/resync を親TUIで反映する。Ctrl+O reservation と management-vs-live input classifier の既存契約を維持する。
- pending / live tab、safe feedback、Closeup tab strip と terminal contents を同じ app state から描画する。attach failure は安全な feedback を表示し、local spawn / local session mutation をしない。
- #257 の session-create form / agent launch の作業と write-set を分離し、同じ Home controller/effect vocabulary を共有する場合も terminal path のみを変更する。

## 対象外

- daemon IPC / PTY/runtime 実装（#264）。
- session create、Agent adapter launch、raw command/argv/env 入力、local fallback。
- terminal copy/search、複数 workspace の新 UX、v1 全面視覚置換。

## 受け入れ条件

- selected root/session の Closeup で terminal action を実行すると daemon に一度だけ launch/attach intent が送られ、既存 terminal がある場合は exact `TerminalRef` へ再attachする。
- TUI は local PTY/process を生成せず、daemon unavailable/stale/orphan/stream gap では typed safe feedback と resync/reconnect policy を表示する。
- detached TUI の再起動後、inventory から saved `TerminalRef` を検証して選択 tab のみを reattach し、name/path lookup や replacement spawn を行わない。
- live terminal input は bytes を一度だけ daemon に送り、resize は geometry dedupe される。Closeup / Overview 操作は live input を奪わない。
- fake daemon client + fake terminal stream の integration test と injected real-PTY regression で launch、detach/reattach、output replay、input、resize、exit、resync を確認する。
- 実装済み UI 操作を `document/01-overview.md` に、IPC/daemon 境界は各正本（#264）へリンクして反映する。
