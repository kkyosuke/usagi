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
updated_at: 2026-07-17T14:22:12.472188+00:00
---

## 目的

#258 の第 3 段階（本体）。実端末のフレームループの state を `AppState` に、入力を `update()` 経由に、描画を `render_home` に差し替え、controller 経路を実 runtime の唯一の Home state / render source にする。この issue の PR で実端末経路が切り替わる。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.2 / §4.5 / §5 PR3。

## スコープ

- `app_event_from_key(Key) -> Option<AppEvent>` 変換を新設する（`Key::Live` → `AppKey`、通常キーは既存 `classify_management_input` を再利用、resize / tick / backend も変換）。
- `Key::Passthrough` の live 入力ゲートを controller state（`route` が Closeup かつ `has_live_pane`）参照に置き換える。
- pointer の暫定 seam: `HomeProjection::row_at(y)` の hit-test と `AppKey::SelectRow(Selection)`（reducer 新設）でクリック選択を実現する。terminal 内 drag / copy は shell + `TerminalSession` に残す。
- 新フレームループ `WorkspaceRuntime`（drain → poll → render → input → dispatch）を実装し、`drive_workspace_with_*` の内部を差し替える。
- controller に相当が無い modal（PR / preview / error）は shell overlay として `render_home` 出力に重ねる暫定接続を維持する。
- `presentation/mod.rs` の runtime テスト群を fake `Terminal` + fake port で新ループの期待値へ移植し、row contract（wrap / Enter / `t` / marker / viewport / empty / tiny geometry）と live terminal 退行（PTY fixture）の integration test を追加する。

## 対象外

- 旧 `Workspace` view の削除（後続の掃除 issue で行う）。
- 右ペイン tab の可視性・layout の変更。

## 完了条件

- 実端末の Home 描画・入力経路が controller projection を経由する（#258 の完了条件を満たす）。
- #295 / #305 の live pane / terminal 挙動が退行しない。
- #287（create entry）が乗れる seam（`+ new session` 活性化・`Overlay::CreateSession`・`Effect::CreateSession` 実行）が実端末で機能する。
