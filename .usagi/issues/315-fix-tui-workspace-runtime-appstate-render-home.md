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
updated_at: 2026-07-17T22:45:36.045879+00:00
---

## 目的

#258 の第 3 段階（本体）。実端末のフレームループの state を `AppState` に、入力を `update()` 経由に、描画を `render_home` に差し替え、controller 経路を実 runtime の唯一の Home state / render source にする。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.2 / §4.5 / §5 PR3。

## 進捗（分割メモ）

着手時に判明したとおり、上流の #313（投影 parity）/ #314（`DaemonBackend` = Effect executor）は設計より土台を残しており、**実端末ループ側には `AppState` / `render_home` / `PaneRuntime` / `DaemonBackend` がまだ一切配線されていない**（`presentation/mod.rs` は旧 `WorkspaceView` + `TerminalSession` のまま）。full swap は複数ファイル・約 71 個の runtime テスト移植・coverage 100% を伴うため、controller 側の前提を先に green PR で確定し、ループ切替本体を残りとして分けた。

### この PR で landed（controller 側前提）

- `presentation::app_event_from_key(Key) -> Option<AppEvent>`：`Key` → controller `AppEvent` 変換。live prefix 解決済みの `Key::Live` を対応 `AppKey` へ、`Key::Other`（resize / backend wakeup）を `Tick` へ写す。
- `AppKey::SelectRow(Selection)` + reducer（pointer 選択の暫定 seam）。存在しない row を指す stale click は無視。
- `HomeProjection::row_at(height, width, column, row) -> Option<Selection>`：`home_left_pane` と同じ幾何で sidebar クリックを hit-test。
- `document/02-architecture.md` に上記 seam を追記。

### 残り（ループ切替本体 = #315 の未了スコープ）

- `PaneRuntime` / agent_runtime / terminal launch を実端末ループへ配線し、旧 `TerminalSession` 機構を置換（または PaneState への変換）。
- 合成ルート（`src/runtime/tui.rs`）で `DaemonBackend` を構築（`TargetStorePort` / `WorkspaceCommandPort` の実 adapter を新設し、agent port を pane runtime に橋渡し）。
- 新フレームループ `WorkspaceRuntime`（drain → poll → render → input → dispatch）を実装し `drive_workspace_with_*` の内部を差し替え。`Key::Passthrough` の live 入力ゲートを controller state（Closeup かつ `has_live_pane`）参照へ。
- PR / preview / error modal を shell overlay として `render_home` 出力に重ねる暫定接続。
- `presentation/mod.rs` の runtime テスト群を fake `Terminal` + fake port で新ループへ移植し、row contract / live terminal 退行（PTY fixture）integration を追加。

## 対象外

- 旧 `Workspace` view の削除（後続の掃除 issue で行う）。
- 右ペイン tab の可視性・layout の変更。

## 完了条件

- 実端末の Home 描画・入力経路が controller projection を経由する（#258 の完了条件を満たす）。
- #295 / #305 の live pane / terminal 挙動が退行しない。
- #287（create entry）が乗れる seam（`+ new session` 活性化・`Overlay::CreateSession`・`Effect::CreateSession` 実行）が実端末で機能する。
