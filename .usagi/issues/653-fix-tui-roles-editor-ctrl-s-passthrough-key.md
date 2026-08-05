---
number: 653
title: fix(tui): Roles editor の Ctrl-S 保存が passthrough_key で握り潰される
status: done
priority: high
labels: [review, v2, tui, input]
dependson: []
related: []
created_at: 2026-08-05T01:01:22.696632+00:00
updated_at: 2026-08-05T08:35:38.017675+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 1。本 issue はその finding を再検証し起票したもの。

## Finding

Home の Roles editor は画面下部に `Ctrl-S: validate + save   Tab: scope   Esc: close` というヒントを表示する（`crates/tui/src/presentation/views/scratchpad_modal.rs` の `render_roles_over`）。reducer 側は `AppKey::SaveRoles` を正しく処理し `Effect::SaveRoles` を発行する（`crates/tui/src/usecase/application/controller.rs` の `update_role_editor`）。

しかし実際の入力経路では Ctrl-S が reducer に届かない。合成ルート `src/runtime/tui.rs` の `passthrough_key` は、`Modifiers` が既定値でも `shift_only` でもないキー（Ctrl 系はすべて該当）を無条件に `Key::Passthrough(bytes)` として返す（フォールバック分岐）。`crates/tui/src/presentation/mod.rs` の `app_event_from_key` は `Key::Passthrough(_) => None` のため、このキーは `AppEvent` に変換されず reducer まで到達しない。

同じ `controller.rs` にある `classify_management_input` には `Char('s')` + control modifier → `AppKey::SaveRoles` という正しい分岐が既に実装されているが、grep で確認した限り本番の入力パイプラインからは一度も呼ばれておらず、呼び出し元はこの関数自身の unit test だけである。つまり修正はおそらく既に書かれているが本番経路に接続されていない。

結果として、**Roles editor から Ctrl-S で保存する経路は本番に存在しない**。Enter は改行追加、Backspace は1文字削除のみで、他の保存手段もないため、画面のヒントは実際には機能しない死んだ機能になっている。

## 影響

- 表示されている保存キーバインドが実際には動作しない。
- 代替の保存手段が無いため、Roles editor は事実上編集内容を保存できない。

## 修正方針（例）

- `passthrough_key`（または `app_event_from_key` 手前の分岐）で、Home overlay が入力を握っている間は Ctrl-S 等の管理用キーを `Key::Passthrough` にせず、既存の `classify_management_input` へ配線する。
- あるいは `classify_management_input` を実際の入力パイプラインから呼び出すよう接続する。
- 他の Home overlay（Env editor 等）に同種の保存キーがあれば同時に確認する。

## 受け入れ条件

- Roles editor で Ctrl-S を押すと `Effect::SaveRoles` が発行され、保存が実行される（実 PTY/統合テストで確認）。
- live pane へのキー転送等、他の passthrough が必要な場面を退行させない。
- dead code だった `classify_management_input` が実際に呼ばれることを検証する unit test を追加する。
