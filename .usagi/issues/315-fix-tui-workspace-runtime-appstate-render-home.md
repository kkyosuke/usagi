---
number: 315
title: fix(tui): 実端末 Workspace runtime を AppState / render_home 経路へ切り替える
status: done
priority: high
labels: [tui, controller, runtime]
dependson: [313, 314]
related: [258, 287, 295, 305]
parent: 258
created_at: 2026-07-17T14:22:12.472188+00:00
updated_at: 2026-07-18T04:11:47.130791+00:00
---

## 目的

#258 の第 3 段階（本体）。実端末のフレームループの state を `AppState` に、入力を `update()` 経由に、描画を `render_home` に差し替え、controller 経路を実 runtime の唯一の Home state / render source にする。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.2 / §4.5 / §5 PR3。方針は「最小 strangler」（legacy live-pane 機構＝`TerminalSession` は daemon IO transport として残し、Home の state/描画/入力だけ controller へ寄せる）。

## 完了

実端末経路（直 `usagi <path>` と Welcome→Open→workspace の両 entry point）が新フレーム
ループ `presentation::drive_workspace_controller` を通り、Home の state=`WorkspaceRuntime`
（controller `AppState` + target-scoped `PaneRegistry`）／入力=`app_event_from_key`→`update()`
／描画=`render_home` に一本化された。legacy `WorkspaceUi` は daemon IO transport として温存。

### 段階（すべて `main` にマージ済み）

- **#1044**: 入力変換 `app_event_from_key` / pointer seam `AppKey::SelectRow` + `HomeProjection::row_at`。
- **#1048**: controller 駆動 runtime `WorkspaceRuntime`（state/pane registry/render/input）。
- **#1052**: `on_effect`（Effect→pane mirror）。
- **#1055 / #1057**: overlay renderer `create_session_modal` / `quit_modal`。
- **#1058**: 投影 seam（`focused_terminal` / view の `metrics()`・`git_diffs()`）。
- **#1060**: フレームループ本体 `drive_workspace_controller` + `dispatch_controller_effect` +
  pane completion 還流 + by-ref terminal IO。
- **#1063**: 合成ルート差し替え（両 entry point を新ループへ）。real PTY e2e の quit を
  controller の Ctrl-Q 契約へ更新。
- **本 PR**: 新ループの integration test（quit / create-entry seam）を追加し完了を固定。

### 完了条件の充足

- 実端末の Home 描画・入力が controller projection を経由する ✓（両 entry point。CI green）。
- #295 / #305 の live pane / terminal 挙動は legacy machinery を無改変で再利用し退行なし ✓。
- #287（create entry）: `+ new session` 活性化・`Overlay::CreateSession`・`Effect::CreateSession`
  実行の seam が実端末（新ループ）で機能する ✓（integration test で固定）。

## 後続（#316）

旧 `Workspace` view と旧ループ（`drive_workspace_with_agent_port_*` / `step_workspace` 系）、
およびそれらを対象とした約 71 個の runtime テストの削除。新ループへ切替済みのため旧経路は
dead-in-prod。Overview（`:`）command palette の controller loop 対応も後続で行う。
