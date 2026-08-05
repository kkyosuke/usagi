---
number: 647
title: fix(tui): Home overlay の入力欄で貼り付け（Paste）が一律破棄される
status: in-progress
priority: medium
labels: [review, v2, tui, input]
dependson: []
related: []
created_at: 2026-08-05T01:01:31.057691+00:00
updated_at: 2026-08-05T08:28:21.317622+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 2。本 issue はその finding を再検証し起票したもの。

## Finding

`crates/tui/src/presentation/mod.rs` の `app_event_from_key` は `Key::Paste(_) => return None` を `Key::Passthrough` / `Key::Pointer` / `Key::Click` と同じ分岐で処理しており、Home 配下のあらゆる入力欄で貼り付けイベントを無条件に捨てている。

影響を受けるのは Overview / Closeup の command palette、Notes、Environment editor（Home overlay 版）、Roles editor、create-session 命名フォームなど、`AppKey` 経由で処理される Home 内の全入力欄である。いずれも `AppKey::Paste` 相当の variant を持たず、`Key::Paste` はそもそも `app_event_from_key` の手前で握り潰される。

一方、Welcome の「New workspace」フォーム（`step_new`）、Open フォーム（`step_open`）、単体の Config 画面（`step_config` の `paste_environment`）は Home に入る前の別経路のため `Key::Paste(text) => ...` を個別に処理しており、貼り付けは正しく機能する。したがって本 issue のスコープは **Home 配下の overlay/reducer に限定**される。

なお `Key::Paste(String)` 自体は合成ルート側（`src/runtime/tui.rs`）で正しく構築されており、テキストは欠落なく届いている。破棄は `app_event_from_key` の一箇所だけで発生している。

## 影響

- Home に入った後は、長い env の値や TOML の role 定義、リポジトリパスなどを貼り付けられず、手打ちを強いられる。
- データ損失はないが、明確な使い勝手の劣化。

## 修正方針（例）

- `app_event_from_key` に `AppKey::Paste(String)`（または同等の event）を追加し、フォーカス中の入力欄のカーソル位置にテキストを挿入する処理を各 reducer（Notes / Env editor / Roles editor / create フォーム等）に配線する。

## 受け入れ条件

- Home 配下の対象入力欄それぞれで、貼り付けたテキストがカーソル位置に反映される。
- 既存の pre-Home（Welcome/Open/Config 単体画面）の貼り付け動作を退行させない。
- 各入力欄について貼り付け反映を検証する unit test を追加する。

## 関連

同じレビューの finding 1（Ctrl-S 無効化）と症状は似ているが、根本原因（合成ルートの `passthrough_key` vs `usagi-tui` 内の `app_event_from_key`）が異なるため別 issue として起票している。
