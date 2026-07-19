---
number: 369
title: feat(tui): session 作成失敗を dialog-style の error modal で提示する
status: done
priority: medium
labels: [tui, feat, ux, modal]
dependson: []
related: []
created_at: 2026-07-19T21:16:58.008438+00:00
updated_at: 2026-07-19T21:31:27.667644+00:00
---

## 背景 / 問題

Home controller から新規 session を作成する経路（Switch の `Ctrl-A` 作成フォーム、および
Overview palette の `session create <name>`）は、いずれも `request_create_session` →
`Effect::CreateSession` → 完了時の `AppEvent::OperationResult` に収束する。

現状、作成 request が daemon で失敗したとき（`OperationResult { succeeded: false }`）は
`state.notice` にだけ safe message を格納する。しかし `HomeProjection::from_state` は
`feedback` は投影するが `notice` を投影しないため、**controller ベースの Home では作成失敗が
一切ユーザーに表示されない**（`AppState::notice()` は controller の unit test でしか読まれていない）。

加えて production の live 経路（`drain_session_completions`）は、`SessionCommandPort::execute`
が返す `Err(safe message)` を **黙って捨てて** おり、controller へ `OperationResult` を還流しない。
このため実 daemon の作成失敗はサイレントに握り潰される。

これは inline validation（作成フォームの必須項目チェックを行の下に表示する既存 UX）とは別問題であり、
本 issue では **accept 後の daemon 失敗 / 安全に表示できる作成エラー** を扱う。inline validation の
表示ロジック（`CreateSessionForm::error` / `create_session_modal` の error 行）には手を入れない。

## ゴール

作成 request の daemon 失敗、および安全に表示できる作成エラーを、Home 背景を残す
confirmation/dialog style の **error modal** で提示する。閉じた後の create 入力・作成状態の扱いを
既存 UX と矛盾させない。raw protocol / internal / secret detail は画面に出さない。

## 変更内容

### reducer（`usecase/application/controller.rs`）
- `Overlay::CreateSessionError` を追加し、表示する safe `Notice` を `AppState` に保持する
  （`create_session_error: Option<Notice>` + accessor）。
- `AppEvent::OperationResult` handler: 除去した pending が `PendingKind::CreateSession` かつ
  `succeeded == false` のとき、他 overlay が開いていない（`overlay.is_none()`）場合に限り
  error modal を開く。overlay が開いている場合は従来どおり `notice` へフォールバックし、
  作成フォームや他 overlay を破壊しない。
- dismissal: `Overlay::CreateSessionError` で `Esc` / `Enter` / `Ctrl-C` を押すと modal を閉じ、
  `create_session_error` を消して Home（Switch）へ戻る。作成フォームや pending の残骸を残さない。

### view（`presentation/views/create_session_error_modal.rs`）
- `quit_modal` と同じ dialog スタイルの stateless renderer。タイトル + safe message +
  `Enter / Esc: dismiss` を `render_over` で Home 背景へ合成する。
- `presentation/mod.rs::render_controller_frame` に overlay 分岐を追加する。

### production 配線（`presentation/mod.rs`）
- `Effect::CreateSession` dispatch 時に controller の `PendingToken` を `WorkspaceUi` に控える。
- `drain_session_completions` が作成の `Err(message)` を受けたら、safe な 1 行へ縮めた
  `Notice`（`new_project_notice` と同じ方針の pure helper）を載せた
  `AppEvent::OperationResult { succeeded: false, created: None }` を controller へ還流する。
  これで実 daemon 失敗時に error modal が発火し、pending も解消される。

## テスト（reducer / render / runtime regression）
- reducer: 失敗 `OperationResult` で modal が開く / dismiss で Home へ戻る / overlay 表示中は
  notice フォールバックする / 表示は safe message だけ、を確認する。
- view: `render_over` が Home 背景の上に title・safe message を描き、幅を超えないこと。
- workspace_runtime / render_controller_frame: 失敗還流 → modal 描画の経路を確認する。
- safe 1 行縮約 helper の pure unit test（複数行・長文・空文字）。

## ドキュメント
- `document/03-tui.md` の overlay 節に、作成失敗の error modal（発火条件・dismiss・safe 表示・
  inline validation との棲み分け）を追記する。

## 非スコープ
- inline validation（フォーム必須項目の行下 error）の変更。
- 作成成功時の landing 経路の変更（既存の snapshot reconcile を維持）。
- 作成入力の retain / 再編集フロー（dismiss は Home へ戻すのみ）。
