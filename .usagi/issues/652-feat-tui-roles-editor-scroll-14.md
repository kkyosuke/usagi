---
number: 652
title: feat(tui): Roles editor に scroll を追加し末尾14行以外も閲覧できるようにする
status: done
priority: low
labels: [review, v2, tui, roles]
dependson: []
related: []
created_at: 2026-08-05T01:02:24.030642+00:00
updated_at: 2026-08-05T09:12:24.253953+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 7。本 issue はその finding を再検証し起票したもの。

## Finding

`RoleEditor`（`crates/tui/src/usecase/application/controller.rs`）は `scope`/`source`/`error`/`loading`/`saving` フィールドのみを持ち、スクロール位置を保持するフィールドが無い。描画側の `render_roles_over`（`crates/tui/src/presentation/views/scratchpad_modal.rs`）は常に `source` の**末尾14行**だけを表示する（`lines().rev().take(14)...`）。

`update_role_editor`（`controller.rs`）が処理するキーは `Escape` / `Tab`(ToggleRoleScope) / `SaveRoles` / `Enter`(改行追加) / `Backspace`(1文字削除) / `Char`(追記) のみで、`Up`/`Down`/`PageUp`/`PageDown` 等のナビゲーションは実装されておらず、それ以外のキーは全て無視される（`_ => Vec::new()`）。

`roles.toml` が14行を超えると、編集中に先頭〜中盤の内容を一切確認できない。編集内容自体は失われないが、閲覧性が大きく損なわれる。

## 影響

- 14行を超える role 定義の編集時、内容の見直しや衝突確認ができない。

## 修正方針（例）

- `RoleEditor` にスクロール位置（先頭行 index 等）を追加し、`Up`/`Down`/`PageUp`/`PageDown` で移動できるようにする。
- カーソル位置に応じて表示ウィンドウを自動追従させる（末尾追記時は現状通り末尾が見えるようにする）。

## 受け入れ条件

- 14行を超える source に対し、スクロール操作で任意の行へ移動して閲覧できる。
- 既存の追記・保存・エスケープ動作を退行させない。
- スクロール挙動の unit test を追加する。
