---
number: 315
title: fix(tui): 実端末 Workspace runtime を AppState / render_home 経路へ切り替える
status: todo
priority: high
labels: [tui, controller, runtime]
dependson: [313, 314]
related: [258, 287, 295, 305]
parent: 258
created_at: 2026-07-17T14:22:12.472188+00:00
updated_at: 2026-07-18T02:24:47.584360+00:00
---

## 目的

#258 の第 3 段階（本体）。実端末のフレームループの state を `AppState` に、入力を `update()` 経由に、描画を `render_home` に差し替え、controller 経路を実 runtime の唯一の Home state / render source にする。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.2 / §4.5 / §5 PR3。方針は「最小 strangler」（legacy live-pane 機構＝`TerminalSession` は残し、Home の state/描画/入力だけ controller へ寄せ、Effect は既存機構＋note usecase に route。DaemonBackend 本番配線・production TerminalPort・environment 永続化は後回し）。

## 進捗（分割）

上流 #313 / #314 は設計より土台を残しており、実ループ側は `AppState` / `render_home` / `PaneRuntime` / `DaemonBackend` が未配線だった。着手時調査で production 側に **PaneRuntime を駆動する TerminalPort 実装・bytes→rows の VT bridge・environment 永続化が存在しない**ことも判明。full swap は複数 PR に分割する。

### landed

- **PR #1044**（controller 側前提）: `app_event_from_key`（Key→AppEvent）/ `AppKey::SelectRow` + reducer / `HomeProjection::row_at` / architecture 追記。
- **本 PR**（切替本体の seam）: `presentation::workspace_runtime::WorkspaceRuntime`。controller `AppState` と target-scoped `PaneRegistry` を所有し、Home の row state・live-pane 可用性・`render_home` フレームの単一 source にする controller 駆動 runtime。handle_key/apply_event、pane lifecycle mirror（request/complete/fail/exit）、select_tab、wants_live_input（passthrough gate）、render を pure reducer だけで実装し fake なしで 100% test。**production frame loop へは未配線。**

### 残り

- `WorkspaceRuntime` を production frame loop（`drive_workspace_with_*`）へ配線し、`step_workspace` / `render_workspace` を置換。Effect を legacy 機構（session create worker / pane_launches / note usecase）へ route。legacy `TerminalSession` の rows を `TerminalViewProjection` に、pane_launches/completions を `WorkspaceRuntime` の pane lifecycle に橋渡し。
- PR / preview / error modal を shell overlay として `render_home` 出力に重ねる暫定接続。
- `presentation/mod.rs` の runtime テスト群（約 71）を fake `Terminal` + fake port で新ループへ移植し、row contract / live terminal 退行（PTY fixture）integration を追加。
- 合成ルート（`src/runtime/tui.rs`）の 2 呼び出し点を新ループへ差し替え。

## 対象外

- 旧 `Workspace` view の削除（後続の掃除 issue で行う）。
- 右ペイン tab の可視性・layout の変更。
- production TerminalPort / VT bridge / environment 永続化 / DaemonBackend 本番配線（最小 strangler では使わない。設計忠実版へ移す際に別 issue）。

## 完了条件

- 実端末の Home 描画・入力経路が controller projection を経由する（#258 の完了条件を満たす）。
- #295 / #305 の live pane / terminal 挙動が退行しない。
- #287（create entry）が乗れる seam（`+ new session` 活性化・`Overlay::CreateSession`・`Effect::CreateSession` 実行）が実端末で機能する。
