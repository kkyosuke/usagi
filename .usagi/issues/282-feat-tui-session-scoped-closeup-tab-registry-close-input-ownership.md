---
number: 282
title: feat(tui): session-scoped Closeup tab registry と close/input ownership を固定する
status: done
priority: high
labels: [tui, pane, terminal, closeup, design]
dependson: []
related: [265, 279]
parent: 227
created_at: 2026-07-13T11:20:25.806247+00:00
updated_at: 2026-07-13T11:31:34.203907+00:00
---

## 目的

Closeup pane の tab 状態を selected session ごとに完全分離する registry として定義し、terminal / Agent tab の作成・選択・close を stable identity のまま還元する。#265 の daemon attach runtime と #279 の controller/Closeup input は、この registry を唯一の tab state contract として利用する。

## 現状の根拠

- `crates/tui/src/usecase/application/pane.rs` の `PaneState` は pending `OperationId` / live `TerminalRef` を stable identity にする純粋 reducer として実装済みだが、selected target ごとの所有・切替を表す registry を持たない。
- `controller.rs` は `LivePaneAvailability(bool)` だけを保持するため、session A の tab 作成・close が session B の Closeup modal / tab state に影響しないことを表現・検証できない。
- #278 は live runtime の tab-gated input ownership を、#279 は controller path への投影を、#265 は daemon-owned terminal launch/attach runtime をそれぞれ扱う。session-scoped state contract を先に固定しないと三経路で状態が分岐する。

## スコープ

- `Target::Session(SessionId)` ごとに独立した `PaneState` を所有する `PaneRegistry`（名称は実装で調整可）を導入する。root target と各 session は同じ registry API を使うが、session identity が異なる state を共有しない。
- tab の作成・completion・restore・selection・exit・close を target-scoped に dispatch する。pending は `OperationId`、live は complete `TerminalRef` で重複排除・選択を行い、表示 label/index を identity に使わない。
- 選択中 tab の close は隣接 tab を安定して選択し、最後の tab を閉じたときだけ target 選択＋空 pane へ遷移する。background session の tab 作成、completion、exit、close は、表示中 session の tab・selected tab・modal visibility を変更しない。
- Closeup modal visibility の authoritative predicate を target-scoped tab 有無と explicit/forced action stateから導く。tab 無しは modal 表示、tab 有りは tab が入力を所有し、明示操作だけが modal を開く。modal 表示中は tab navigation/close/passthrough と競合しない command routing contract を effect/event で表す。
- daemon launch、IPC wire、PTY spawn、実 renderer/event pump は変更しない（#265）。live runtime の prefix 実装・controller renderer への接続は #278/#279 が行う。

## 受け入れ条件

- session A を選択して terminal/Agent tab を作成・選択・close しても、session B の tabs、selected tab、pending operation、Closeup modal state は一切変化しない。A に戻ると A 固有の state が復元される。
- tab selection は index/label ではなく pending `OperationId` または full `TerminalRef` で維持され、同一 terminal completion、background completion、exit、restore が別 session の state を奪わない。
- selected tab の close は次の tab（末尾なら直前）へ選択を移し、最後の tab の close は target selection と空 pane に戻る。close は daemon-owned terminal の kill intent を発行しない。
- tab 有無と explicit action modal state の組合せを table-driven reducer test で網羅し、modal が入力を所有する間は tab 操作が dispatch されず、tab が所有する間は通常 Closeup modal が自動表示されないことを確認する。
- 複数 session の create/select/close、pending completion、exit、Closeup modal open/close を通した reducer/integration regression を追加し、coverage 100% を維持する。
- 実装後、tab の identity・session scope・close・input ownership の仕様を `document/03-tui.md` の Closeup pane 正本へ更新する。

## 依存と後続

- 本 issue は pure application state と tests を所有する。
- #265 は daemon launch/attach と renderer/runtime integration をこの registry に接続する。
- #279 は controller reducer path の tab-gating と modal projection をこの registry に接続する。
