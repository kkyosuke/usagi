---
number: 361
title: fix(tui): 新規 session 作成を profile/model modal から左サイドバー inline の name-only 入力へ戻す
status: done
priority: high
labels: [tui, controller, session, ux]
dependson: []
related: [287, 315, 258]
created_at: 2026-07-19T20:50:30.417358+00:00
updated_at: 2026-07-19T21:12:30.458042+00:00
---

## 背景

`+ new session` を活性化したときの作成 UI は、現在 `Overlay::CreateSession` として画面中央に重なる **3 項目 modal**（name / profile / model、`modal::render_over`）で描かれる。この modal renderer は #315（#1055）で追加され、実端末 controller ループ（`presentation::drive_workspace_controller`）が唯一の Home state/描画/入力になった時点から production 経路に載っている。

- 描画: `crates/tui/src/presentation/views/create_session_modal.rs`（中央 modal、3 field、`(workspace default)` placeholder、Tab で field 巡回）。
- 合成: `crates/tui/src/presentation/mod.rs` が `create_session_form()` が `Some` のとき `create_session_modal::render_over` を Home frame へ composite（`mod.rs:1481` 付近）。
- state/reducer: `CreateSessionField { Name, Profile, Model }` と `CreateSessionForm { name, profile, model, field, error }`、`next_field()`（Tab 巡回）、`update_create_session_form`（Tab/Backspace/Char/Enter/Escape）— すべて `crates/tui/src/usecase/application/controller.rs`。

しかし v1 と本来の UX は、**左サイドメニュー内で `+ new session` 行そのものが name-only の inline 入力になり、名前を打って Enter すれば session が作られる**方式だった。profile/model の先行入力は不要で、workspace の default policy に委ねる。現行の 3 項目 modal は入力ステップと画面遷移を増やし、「名前だけ打って Enter」で確実に作成される元の軽い流れを損なっている。

`SessionCreateIntent { name, profile: Option, model: Option }` は既に profile/model が省略可（空＝daemon の workspace default policy）なので、TUI から profile/model を常に `None` にしても daemon port / typed lifecycle は無改変で済む。

## 目的

新規 session 作成 UI を、中央 modal の 3 項目フォームから、**左サイドバーの `+ new session` 行に inline 展開する name-only 入力**へ戻す。名前だけを入力して Enter すれば、daemon-authoritative な `Effect::CreateSession` が 1 回だけ確実に dispatch される。profile/model の先行 UI はこの作成フローから除去する。

## スコープ

- **state 簡素化**: `CreateSessionForm` を `{ name: String, error: Option<Notice> }` に縮小し、`CreateSessionField` / `profile` / `model` / `next_field()` を削除する。`request()` は `SessionCreateIntent { name, profile: None, model: None }` を返す。`SessionCreateIntent` の型（profile/model は `Option`）は変えず、daemon 側の allowlist/policy は無改変で残す。
- **reducer**: `update_create_session_form` から Tab（field 巡回）を除き、Backspace/Char で name を編集、Enter で name-only 送信、Escape で cancel とする。空名は従来どおり `form.error` に validation を付け、再 submit まで作成しない。`Ctrl+A` / `Home` が form 所有中に作成を再発火しない不変条件は維持する。
- **入力所有の gate**: `Overlay::CreateSession` は「Home 左ペインの `+ new session` に対する入力」を表す input-owner state として残す（この enum の doc は既に inline を示唆）。描画だけを modal から inline へ移す。
- **描画（inline 化）**: `Overlay::CreateSession` が開いている間、左サイドバーの `+ new session` 行を **name の block caret を持つ inline 入力**として描く。form draft（name / active / error）を `HomeProjection` に thread して `render_home` / `workspace.rs` の sidebar 行描画で使う。`create_session_modal::render_over` の composite（`mod.rs`）を撤去し、`create_session_modal.rs` と module 登録を削除する。
- **daemon-authoritative の維持**: `Effect::CreateSession` → `SessionCommandPort::create` → `SessionLifecycleAdapter::submit`（`OperationId` 保存・blind-retry しない typed lifecycle）→ daemon が実 `SessionId` を割当、TUI は `PendingRow::Creating` skeleton を表示、という経路を一切変えない。local store/worktree/PTY fallback を追加しない。
- **live PTY input ownership の維持**: Closeup の Ctrl+A / action overlay、live pane の Ctrl+A passthrough、Ctrl-O の mode ルール、`PaneInputOwner` の tab routing を無改変で保つ。

## 対象外

- `+ new session` 行の entry 活性化・Ctrl+A の 3 表現（control byte / Ctrl+キー / Home decode）・empty sidebar での常設という affordance の restore（#287 の担当。本 issue は「活性化後のフォーム形状」を扱う）。
- sidebar の row order / viewport / right-pane tab layout の再設計（#258）。
- daemon lifecycle / IPC wire / worktree 作成 worker の変更。
- profile/model を daemon 側で選ぶ将来 UX（本 issue はあくまで作成フローからの除去）。

## 受け入れ条件

- `+ new session` を活性化すると、中央 modal ではなく左サイドバーの当該行が inline の name 入力（block caret）になる。profile/model の field / placeholder / Tab 巡回は表示されない。
- name を打って Enter すると、`Effect::CreateSession { intent: { name, profile: None, model: None } }` が **1 回だけ** dispatch され、pending / feedback / safe landing / daemon 権威（`PendingRow::Creating`→daemon 割当 `SessionId`）が従来どおり機能する。
- 空名で Enter しても作成されず、inline に validation（`form.error`）が出る。Escape は作成せず戻る。
- form 所有中の Ctrl+A / Home は新しい form や effect を発生させない。Closeup / live pane の Ctrl+A・Ctrl+O・tab routing は無改変。
- `create_session_modal.rs` は削除され、`mod.rs` は modal を composite しない。`CreateSessionField` / profile / model は controller から除去される。

## テスト

- reducer: `+ new session` 活性化で `Overlay::CreateSession` かつ name-only form（profile/model field が無い）になること、Char/Backspace で name 編集、Enter→`Effect::CreateSession` が profile=None・model=None で 1 回、空名 Enter で error 付与かつ effect 0、Escape で cancel。
- 入力不変条件: form 所有中の Ctrl+A / Home が no-op、Closeup Ctrl+A / live-pane Ctrl+A passthrough / Ctrl-O が無改変であること。
- 描画: 左サイドバーの `+ new session` 行が inline 入力（typed name + caret）として描かれること、狭い geometry の runtime frame regression、modal が composite されないこと。
- runtime/loop: controller ループ（`drive_workspace_controller` 経路の integration test）で `+ new session` 活性化→name 入力→Enter→単一 create effect の seam が inline で機能すること。fake daemon lifecycle port で `OperationId` 保存の typed 送信を回帰する。

## 関連

- #287（`+ new session` の entry affordance / Ctrl+A wiring。profile/model UX は #287 の対象外なので本 issue と補完関係）
- #315 / #258（controller runtime 移行。create-entry seam を実端末へ載せた本体。本 issue はその seam の描画形状のみを変更）
