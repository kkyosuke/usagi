---
number: 303
title: feat(tui): Closeup terminal の出力表示と入力送信を daemon PTY へ双方向接続する
status: done
priority: high
labels: [tui, terminal, ipc]
dependson: []
related: [265, 255, 264, 270, 235]
created_at: 2026-07-14T22:35:02.676972+00:00
updated_at: 2026-07-15T00:07:50.906672+00:00
---

## 背景

`usagi v2` は daemon-owned generic shell terminal を安全に起動・所有する経路（#255 / #264 / #270）を実装済みで、Unix IPC 越しに実 PTY の `launch → attach → input → output → detach → reconnect` が動作する。TUI 側も #265 で Closeup の `terminal` action / command から daemon への **launch** intent 送出を接続した（`src/runtime/tui.rs::DaemonAgentCommandPort::launch_terminal`）。

しかし実ユーザー操作としての「TUI から terminal を開き `ls` を実行して結果を見る」は**成立しない**。現行の実行時 TUI（`presentation::run_workspace…` が駆動する `WorkspaceView` ループ）で欠けているのは次の 3 点である。

1. **attach / 出力取得がない**: launch 後に daemon terminal へ attach せず、出力を取得していない。
2. **PTY 出力の描画がない**: 右ペイン（`views/workspace.rs::home_right_pane`）は live tab のラベルと phase / feedback 行だけを描画し、PTY 出力面を持たない。
3. **入力送信がない**: `src/runtime/tui.rs::read_key` は `LiveInputOutput::Passthrough(_)` の raw bytes を破棄し（"future PTY passthrough" コメントのまま）、`Key` に生バイトを載せる variant が無いため keystroke が terminal へ届かない。

`crates/tui/src/usecase/application/pane_runtime.rs` の streaming engine（`PaneRuntime` / `TerminalPort`）は attach/stream/input を実装済みだが、実バイナリ（`src/runtime/tui.rs`）からは構築されず、`stream_event` は **push 型**である。一方、TUI が使う同期 `IpcClient::request`（`crates/core/src/usecase/client.rs`）は **`Event` frame を破棄**して一致する `Response` だけを返すため、daemon の terminal output は push では届かない。**出力は `Attach` の snapshot と `Resume { after_offset }` の poll で取得する**設計に合わせる必要がある。

## スコープ（実 `ls` を成立させる TUI 双方向接続）

- **VT screen grid**（`usagi-tui`, 純粋）: daemon の raw PTY 出力バイト列を、幾何（行×桁）に沿った表示行へ変換する最小 VT emulator を追加する。印字・`\n` / `\r` / `\b` / `\t`・行折返し・カーソル移動（CUP/CUU/CUD/CUF/CUB）・消去（EL/ED）・SGR 無視・overflow scroll を扱う。
- **polling terminal session coordinator**（`usagi-tui`, 純粋）: `TerminalStreamPort`（`attach` / `poll(after_offset)` / `input(subscription, seq, bytes)` / `resize` / `detach`）に対して、subscription・出力 offset・input sequence を fence しつつ VT grid を更新する `TerminalSession` を追加する。gap 検知時は attach snapshot で置換する。
- **WorkspaceView 統合**: 選択中 live terminal tab に対して attach → poll → 描画 → 入力送信を結線する。右ペインに VT grid を描画し、既存の tab strip / phase / feedback と同じ app state から出す。
- **合成ルート**（`src/runtime/tui.rs`, 実 IO は `#[coverage(off)]`）: `TerminalStreamPort` を `IpcClient`（`Attach` / `Resume` / `Input` / `Resize` / `Detach`）上に **poll ベース**で実装する。live tab 確定時に attach、ループ tick ごとに poll、live terminal focus 中の passthrough bytes を input として送る。`Key::Passthrough(bytes)` を導入して生バイトを routing する。

## 安全境界（維持する既存設計）

- IPC には任意の argv / cwd / env を追加しない。client は stable profile ID（`login-shell`）と fence 済み scope・geometry だけを送る。program / cwd / env は daemon が trusted に解決する（#255）。
- client 側 fallback spawn をしない。daemon unavailable / stale / orphan / stream gap は typed safe feedback にとどめ、local PTY を作らない。
- detach は subscription だけを外し、PTY/process を殺さない。

## 対象外

- terminal resize の完全対応（初期 geometry は 80x24 固定。resize 追従は後続）。
- copy / search / scrollback UI、複数同時 live terminal の新規 UX。
- workspace-root（session なし）terminal（daemon が現状 `OwnershipUnknown` で拒否）。
- Agent runtime / phase hook / MCP injection（generic terminal とは独立）。

## 受け入れ条件

- 選択中 session の Closeup で terminal action を実行すると、daemon-owned login shell が起動し、右ペインに PTY 出力（shell プロンプト）が描画される。
- live terminal focus 中に `ls` と Enter を打つと、bytes が一度だけ daemon terminal へ送られ、続く poll で `ls` の出力が右ペインに現れる。
- TUI は local PTY/process を生成しない。daemon unavailable / stale / orphan では typed safe feedback を表示する。
- VT screen grid と polling coordinator は fake port を使う純粋テストで、attach / 出力反映 / gap→再取得 / 入力 seq / detach を検証し、カバレッジ 100% を維持する。

## テスト方針

- **pure**: `TerminalScreen`（print / CR / LF / BS / HT / wrap / EL / ED / cursor move / SGR ignore / scroll）と `TerminalSession`（attach snapshot 反映・poll での増分適用・offset gap での snapshot 置換・input seq 増加・detach・safe error）を table / fake-port テストで検証する。
- **composition**: 実 IPC / 実 PTY を叩く経路は `#[coverage(off)]` とし、注入境界（`TerminalStreamPort`）の fake で振る舞いを検証する。

## ドキュメント更新

`document/01-overview.md` / `document/03-tui.md` の terminal UX を、実装済みの「起動 → 出力表示 → 入力実行」に更新する。IPC/daemon 境界は既存正本（#264 / #270）へリンクする。

## 関連

- #265（Closeup terminal を daemon attach runtime へ接続する）: launch のみ実装済み。本 issue はその attach/出力/入力の残りを実ユーザー操作として完成させる。
- #255 / #264 / #270: daemon 側 generic terminal（実装済み・本 issue の消費先）。
- #235: daemon terminal inventory/stream と pane reattach（実装済み）。
