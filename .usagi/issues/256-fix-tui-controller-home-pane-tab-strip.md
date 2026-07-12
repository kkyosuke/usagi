---
number: 256
title: fix(tui): controller Home の右ペインへ pane tab strip を投影する
status: done
priority: high
labels: [tui, bug, render, pane]
dependson: []
related: [225, 232, 235, 238, 245, 246]
created_at: 2026-07-12T23:27:17.697286+00:00
updated_at: 2026-07-12T23:27:23.994628+00:00
---

## 概要

v2 TUI の Home 右ペインで、Closeup の tab strip が表示されず、tab の選択状態を確認・操作できない。旧 `Workspace` renderer は `Preview / Terminal / Diff / Notes` を `tab_menu` で描画するが、controller 系の `HomeProjection` は active target/cwd/agent phase/feedback だけを保持し、`PaneState` / `PaneRuntime` の `PaneTab`・`PaneSelection` を描画へ投影していない。

あわせて実行経路を確認した結果、現行の対話起動は `presentation::drive_workspace_with_overlay_data` → `WorkspaceView::new` → `render_workspace` → 旧 `workspace::render` を通る。一方、`render_home` は現時点で unit/parity test からのみ呼ばれ、controller / pane reducer / live input classifier との runtime composition は未接続である。したがって「controller Home の tab strip を直す」だけでは実画面の入力・描画整合性を保証できない。

## 最小再現

1. workspace を対話 TUI で開き、session を active target にして Closeup を開く。
2. terminal/agent pane を作成・復元して、pane reducer に live tab または pending tab がある状態にする。
3. 右ペインを確認する。

現状:
- controller 側の `render_home` は tab strip を描かず、`[Closeup] active pane` と target/cwd/phase/feedback だけを描く。
- `Ctrl-O n/p`（または左右）を分類する `LiveInputClassifier` と `PaneRuntime::dispatch(PaneEvent::Select(...))` を結ぶ runtime dispatch がない。
- 実際の対話画面は legacy `WorkspaceView` の固定 tab state を描くため、controller/pane reducer が持つ terminal/agent/pending tab は画面に反映されない。

## 期待挙動

- controller を正規の Home runtime として合成したとき、右ペイン上部に tab strip を常時表示する。
- strip は `PaneState` の stable identity を表示専用の label へ変換し、選択中の `PaneSelection::Tab` を accent、非選択 tab を dim で描く。pending は resolving/starting、live は Terminal/Agent を区別できる安全な表示にする。
- target 選択中（tab 未選択）、tab が 0 件、active target の切替、exit/失敗/reconnect でも strip と content が矛盾せず、identity は label/index に依存しない。
- reserved live input の next/previous tab と Closeup の左右キーは pane reducer の selection を更新し、更新された selection が同じ frame で strip と右ペイン content に反映される。terminal への passthrough は一回だけのまま維持する。
- legacy renderer を残す場合は controller runtime が実画面に到達しない理由を解消する。正規経路を controller runtime へ切り替えるか、旧 renderer を明示的な adapter にして tab/projection の二重所有をなくす。

## 修正範囲

- `crates/tui/src/presentation/views/workspace.rs`
  - `HomeProjection` に pane projection（tab list・selected tab/target・必要なら安全な pane status）を追加し、`home_right_pane` を header → tab strip → selected pane content → feedback/footer の一貫した構成にする。
  - tab label と selected state の変換を presentation 専用に置き、`TerminalRef` / `OperationId` を表示名や index に置換しない。
- controller/runtime composition
  - `AppState`、`PaneRuntime`、`PaneState` の所有関係を 1 箇所にまとめ、snapshot/stream/exit/reconnect と Home render に同一 state を渡す。
  - `LiveTerminalAction::{NextTab, PreviousTab, CloseTab, Agent}` と management-mode のキーを pane event/effect へ接続し、active target 変更時の `PaneSelection` reconciliation を定義する。
  - 現在の `run_workspace` / `drive_workspace_with_overlay_data` が旧 `WorkspaceView` を選ぶ経路を controller runtime に接続する。移行時に modal/overlay の既存挙動を後退させない。
- この issue では daemon protocol・PTY broker・terminal byte renderer を変更しない。既存 `TerminalPort` / `PaneRuntime` の境界を利用する。

## 回帰テスト

- render unit/golden:
  - live Terminal + Agent + pending tab を含む Home frame で strip、active style、selected pane label を固定する。
  - target selection、empty tabs、pending success/failure、exit、snapshot reconciliation、CJK/wide label、tiny geometry を含める。
  - ANSI を除去した golden と `display_width <= width` を検証する。
- reducer/runtime fake:
  - next/previous/close と active target 切替が stable `TabSelection` を維持し、renderer projection と一致することを table-driven scenario で確認する。
  - terminal input は reserved action が passthrough されず、通常入力は selected live terminal へ一回だけ送られることを確認する。
- runtime/PTY:
  - controller Home が実際の `run_workspace` 経路で描画される統合テストを追加する。
  - 実 PTY は alternate-screen/resize/reattach を既存 coverage と重複させず、tab strip の初回表示・tab 切替後の frame diff・detach/restore を最小の deterministic case で検証する。純粋 frame + fake runtime で保証できる場合は PTY suite を広げない。

## 完了条件

- controller-based Home を通る対話 TUI で右ペインに tab strip が表示され、pane reducer の選択と一致する。
- tab selection の入力・reducer・projection・render の接続がテストで固定される。
- legacy と controller のどちらが runtime の正本かが 1 経路に整理され、tab state の二重所有がない。
- 実装 PR には本 issue の scope 外の daemon/PTY protocol 変更を含めない。
