---
number: 371
title: fix(tui): Ctrl+Q quit 確認を共通 Yes/No confirmation renderer に統一しボタン表示する
status: done
priority: medium
labels: [tui, bug, modal, quit, confirmation, parity]
dependson: []
related: []
created_at: 2026-07-19T21:44:14.946838+00:00
updated_at: 2026-07-19T22:03:55.676411+00:00
---

## 背景 / 目的

Home の Ctrl+Q（および live pane からの `OpenQuitConfirmation`、live pane 上の Ctrl+C）で開く detach 確認 modal（`Overlay::QuitConfirmation`）は、`views::quit_modal::render_over` が `y: detach` / `n / Esc: stay` のテキストを直描画している。一方 workspace unregister 確認は共通の Yes/No confirmation renderer（`widgets::modal::render_confirmation_over` + `ConfirmationModal` / `ConfirmationView` / `confirmation_buttons`）を使い `[ yes ] [ no ]` ボタンと `←→/Tab` 選択・shortcut 行を出す。quit 確認だけがこの共通部品から外れており、見た目と操作系が不統一。

quit 確認を共通 confirmation renderer に寄せ、`[ yes ] [ no ]` ボタン・選択状態・shortcut 行を出すよう統一する。

## 現状調査

| 項目 | 現状 |
|---|---|
| overlay state | `Overlay::QuitConfirmation`（選択状態を持たない stateless variant） |
| reducer | `update_overlay` の `QuitConfirmation` arm: `y/Y/Enter`→`Detach`、`n/N/Esc`→閉じる。`CtrlC/CtrlQ` は上位で swallow |
| 描画 | `quit_modal::render_over` が `modal::render_over` にテキスト body を直渡し |
| 共通部品 | `widgets::modal` に `ConfirmationModal`（confirm_selected の Copy state）/ `ConfirmationView` / `confirmation_buttons` / `render_confirmation_over` があり、`views::open` の unregister 確認が使用 |
| key 経路 | overlay 表示中は `wants_live_input`=false のため key は `app_event_from_key`→reducer に届く。`Key::Left`/`Key::Right` は現状 `None`（reducer 未到達） |

`ConfirmationModal` は presentation layer（`widgets/modal.rs`）にあるため、usecase layer の `AppState` に直接持たせると依存方向が逆流する。よって reducer は `bool`（confirm 選択）で選択状態を保持し、presentation 側で `ConfirmationModal` に変換する。

## 受け入れ条件

- `quit_modal::render_over` を `modal::render_confirmation_over` 経由に置き換え、`[ yes ] [ no ]` ボタンと shortcut 行（`Enter/y: yes   Esc/n: no   ←→/Tab: choose`）を出す。`Quit` title・`Detach from this workspace?` heading は維持。
- reducer（`AppState`）に quit 確認の選択状態を持たせ、overlay を開く度に Yes を初期選択にリセットする。
- reducer の操作を整合させる:
  - `y/Y`: detach（confirm）
  - `n/N` / `Esc`: stay（cancel）
  - `Enter`: 選択中のボタンを確定（Yes 選択なら detach、No 選択なら stay）
  - `←`/`→` / `Tab`: Yes/No の選択を切り替え
  - `CtrlC` / `CtrlQ`: 既存どおり swallow（modal を閉じない）
- `AppKey::Left` / `AppKey::Right` を追加し `app_event_from_key` で `Key::Left`/`Key::Right` を対応付ける（quit 確認以外では inert）。
- controller loop で実際に Ctrl+Q を押した frame に `[ yes ]` / `[ no  ]` ボタンと shortcut が描かれる regression test を追加する（`render_controller_frame` と `run_workspace_controller` 経路の両方）。
- reducer の選択・確定・キャンセル遷移の単体テストを追加（既存 `management_ctrl_q_always_confirms_and_confirmation_can_cancel` 等を拡張）。
- カバレッジ 100% を維持。

## 非対象

- unregister 確認など他 confirmation の挙動変更。
- 新規 overlay / mode / レイアウトの追加。
- daemon 側の terminal / operation 停止（quit 確認は従来どおり TUI-local な detach のみ）。
