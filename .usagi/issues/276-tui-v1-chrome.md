---
number: 276
title: 実行時 TUI を v1 左右ペイン・Chrome 風タブへ接続する
status: done
priority: high
labels: [tui, runtime, parity]
dependson: []
related: []
created_at: 2026-07-13T02:15:18.166057+00:00
updated_at: 2026-07-13T02:23:53.453717+00:00
---

## 背景

#267 は Chrome 風右ペイン tab widget を純粋描画部品として追加したが、現在の `cargo run` が通る composition root（`src/runtime/tui.rs`）から workspace/controller/pane runtime へ接続されていなかった。そのため実行画面は v1 の Home UI と異なり、session / agent / terminal の tab が一貫して見えなかった。

## 現状との差分

- v1 は左の session sidebar と右の closeup/live-pane を常時 1 桁 divider で並べ、右上に session header と Chrome 風 tab strip を表示する。
- v2 は pane reducer と #267 の `session_tab` widget を持つ一方、実 runtime は従来 workspace renderer を起動しており、pane state を実 frame に投影していなかった。
- daemon lifecycle / session snapshot refresh / agent・terminal attach の既存統合を壊さず、実行経路で同じ state を選択・表示・切替・close できる必要があった。

## 受け入れ結果

- [x] `cargo run -- <workspace>` の実 TUI が v1 に近い左右 pane（session sidebar / right pane）を表示する。
- [x] 右ペインは Chrome 風 tab（形状、選択 marker、close affordance）を表示し、tab が無いときは mascot と案内の空状態を表示する。
- [x] session / agent / terminal tab が controller・pane runtime の実 state から描画され、キーボード操作で選択・切替・close が一貫する。
- [x] daemon-owned terminal の snapshot/stream は既存 `PaneRuntime` の fenced attach/resync 経路を維持し、session snapshot refresh は同じ PR に取り込んだ。
- [x] renderer/view の視覚回帰テストと reducer/runtime の回帰テストを追加し、TUI 仕様（`document/03-tui.md`）を実装済みの挙動へ更新した。
- [x] Rust full gate・coverage・Markdown link check を通した。

## 非目標

v1 の全モーダル・マウス drag/reorder の完全移植や daemon protocol の再設計は本 issue に含めない。
