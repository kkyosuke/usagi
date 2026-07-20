---
number: 373
title: refactor(tui): confirmation modal を shared primitive に統一する（Quit / open unregister を parametrized ConfirmationView へ）
status: done
priority: medium
labels: [tui, ui, modal, refactor]
dependson: [372]
related: [261, 317]
created_at: 2026-07-19T22:00:36.754451+00:00
updated_at: 2026-07-20T01:19:52.482110+00:00
---

## 目的

`widgets/modal.rs` は Yes/No confirmation の共通部品（`ConfirmationModal` state・`ConfirmationView`・`confirmation_buttons`・`render_confirmation_over`）を持つが、実際に共通 renderer を使うのは `open.rs` の "Unregister workspace" 1 経路だけである。Home の `Overlay::QuitConfirmation` は `y: detach` / `n / Esc: stay` の bespoke prompt を手組みし、`open.rs` の "Remove missing registry entries? y/n" も別の手組みになっている。confirmation の描画経路を 1 本化し、Quit も共通 renderer で描く。

設計の正本: `.agents/designs/372-modal-component-refactor.md`。

## 背景

- `ConfirmationModal`（state）は Quit と open で reducer 経由に再利用されているが、その **renderer** は Quit では使われていない。
- 共通 `render_confirmation_over` は現状 `[ yes ]` / `[ no ]` ボタンと `Enter/y … Esc/n … ←→/Tab` footer を hardcode しており、Quit の `y: detach` / `n / Esc: stay`（単一キー hint・detach 語彙）とは見た目が異なる。
- そのまま Quit を共通 renderer に載せ替えると **表示が回帰する**。回帰させないために、共通 confirmation をラベル・キー hint・role の parametrized component に一般化してから移行する。

## スコープ

- `ConfirmationView` を拡張し、button ラベル（既定 `yes`/`no`）・footer キー hint 文字列・confirm/cancel の role・見出し/本文を呼び出し側が指定できるようにする。単一キー hint の compact variant（Quit 相当）も 1 経路で表現する。
- `QuitConfirmation` の view を `render_confirmation_over`（拡張後）へ移行し、現行の copy（`Detach from this workspace?` / `y: detach` / `n / Esc: stay`・danger 強調）を parametrization で保持する。
- `open.rs` の unregister / cleanup の手組み prompt を共通 renderer に寄せられるか監査し、寄せられる分は移行する。
- footer 行は #372 の共通 `footer` helper と整合させる。

## 完了条件

- Home の Quit を含む confirmation が単一の共通 renderer 経路で描かれる。
- Quit の表示（見出し・選択肢 copy・danger 強調・キー hint）は移行前後で回帰しない（frame 一致 test で固定）。
- reducer の Yes/No 選択・Enter/Esc/y/n の挙動は不変（既存 controller test を維持）。
- coverage 100% を維持する。
- `document/03-tui.md` の confirmation に関する記述を実態へ更新する。

## 対象外

- 幅・配色以外の入力 semantics 変更、daemon request 変更。
- `remove_modal` は multi-select list であり confirmation component の対象にしない（list component 側 issue で扱う）。
- #372 で導入する body-composition helper 自体の追加。
