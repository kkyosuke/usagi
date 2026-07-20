---
number: 399
title: feat(tui): Overview の env コマンドを environment editor の本番永続化まで接続する
status: todo
priority: high
labels: [tui, overview, controller, environment]
dependson: []
related: [244, 314, 340]
created_at: 2026-07-20T04:42:47.419318+00:00
updated_at: 2026-07-20T04:42:47.419318+00:00
---

## 背景

Overview modal の `env` コマンドは reducer/editor/effect レベルでは既に構築済みだが（#244 の overlay、#314 の `DaemonBackend`）、**本番経路では no-op のまま**で、editor は空の loading 状態から進まず Save も何も永続化しない。調査で判明した具体的なギャップ:

- **本番 executor が env effect を捨てている**。実行時ループは `run_workspace_controller` → `dispatch_controller_effect`（`crates/tui/src/presentation/mod.rs:1846-1858`）で、`Effect::LoadEnvironment` / `Effect::SaveEnvironment` は空アーム `{}` に落ちる（コメント「needs a daemon store port this loop does not yet inject」）。#314 の `DaemonBackend`（`crates/tui/src/usecase/application/daemon_backend.rs`）は env を `TargetStorePort` に routing するが**テストでしか構築されておらず本番ループには未接続**。
- **`TargetStorePort` の本番実装が存在しない**。実装は test 専用 `FakeStore`（`daemon_backend.rs:460-506`、load は固定値・save は常に `EnvironmentError`）のみ。
- **core に environment 永続化が無い**。`WorkspaceState`（`crates/core/src/domain/workspace_state/mod.rs`）は `sessions` と `root_notes: Scratchpad` のみで environment フィールドが無い。`Settings`（`crates/core/src/domain/settings/mod.rs`）にも env は無い。daemon の `DispatchToolAction`（`crates/core/src/usecase/client.rs:153`）にも環境用の action は無い。
- **reducer に delete 操作と saving/double-submit ガードが無い**。`AppKey::SetEnvironment`（insert/replace）と `AppKey::SaveEnvironment` はあるが、entry 削除の key が無く、`EnvironmentEditor`（`controller.rs:260-265`）に in-flight/saving flag も無いため Save を連打できる。
- **`env` が余分な引数を黙殺する**。`submit_overview`（`controller.rs:2933`）は `Ok(overview::Command::Env { .. }) => open_environment(state)` で `arguments` を無視するため、`env foo` でも editor が開く。

既に動いている reducer 側の資産（変更の土台）:

- `Effect::LoadEnvironment { target }` / `Effect::SaveEnvironment { target, entries }`（`controller.rs:1390,1392`）
- `BackendEvent::EnvironmentLoaded { target, entries }` / `EnvironmentError { target, error }`（`controller.rs:1310,1315`）、target 一致で reduce（`controller.rs:2318-2336`）
- `open_environment`（`controller.rs:2812-2818`）が `EnvironmentEditor::loading(target)` を立て `LoadEnvironment { target }` を発行。`Escape` で `environment_editor = None`。
- overlay 描画 `render_environment_over` / `environment_body`（`crates/tui/src/presentation/views/scratchpad_modal.rs:100-159`）
- 参考にすべき本番 bridge 型: `DaemonDecisionCommandPort`（`src/runtime/tui.rs:77-188`）と `DecisionPort` の注入。session effect を本番ループへ繋いだ先行例が #340。

## 目的

Overview の `env` を、現在 active な workspace root / session target の environment editor を開き、**単一の durable な正本から環境変数を読み込み、追加・編集・削除・保存を実際に永続化する**production 経路まで接続する。reducer の `LoadEnvironment` / `SaveEnvironment` を実端末の effect executor へ配線し、no-op を解消する。

## 永続化の正本（設計判断）

environment の daemon IPC endpoint は存在せず、新設は本 issue のスコープに対して過大。したがって **notes/todos/decisions と同じ `WorkspaceStateStore`（`state.json`）を正本**とし、`crates/core/src/usecase/note.rs` を鏡写しにする。target キー（`Target::Root` / `Target::Session(name)`）で per-target に持ち、workspace root と各 session を独立に編集・保存する。daemon protocol / `DispatchToolAction` は拡張しない（notes と同格の client 書き込み）。

## スコープ

- **core 永続化**: `WorkspaceState`（および per-session state）に environment（`name -> value` の順序安定 map）フィールドを追加し、`usecase/note.rs` と同型の `usecase/environment.rs`（load / set / remove / save、`Target` 受け）を新設。`WorkspaceStateStore` 経由で `state.json` に永続化する。既存 `state.json` に env が無い場合は空として読む（`serde(default)` で後方互換）。
- **本番 store port と配線**: `TargetStorePort` の env メソッド（`load_environment` / `save_environment`）の本番実装を `src/runtime/tui.rs` に置き（`DaemonDecisionCommandPort` と同型）、`run_workspace_controller` の port 群へ注入する。`dispatch_controller_effect`（`presentation/mod.rs:1846-1858`）の `Effect::LoadEnvironment` / `SaveEnvironment` を空アームから外し、store を呼んで結果を `BackendEvent::EnvironmentLoaded` / `EnvironmentError` として reducer へ還流する。「effect → 実行 → event 還流」の単方向を保つ。
- **delete 操作**: entry 削除を reducer に追加（例 `AppKey::RemoveEnvironment { name }` → editor から entry を除去、error クリア）。editor 入力の add/edit/delete が一通り reducer で表現できること。
- **loading / saving 状態と二重送信防止**: `EnvironmentEditor` に in-flight（saving）状態を持たせ、既存 UI 規約（New/Clone form の `PendingOperation` / `pending()` ガード。参考 `controller.rs` の `new_submit_while_pending_ignores_the_duplicate_operation`）に沿って saving 中の再 Save を no-op にする。`environment_body` に loading/saving の表示を追加する。
- **失敗・再試行**: load / save 失敗時は editor に留まり、入力（entries）を失わず safe error を表示、再 Save 可能にする（`EnvironmentError` は既に entries を保持したまま error をセットするので、それを saving 状態解除と組み合わせる）。
- **余分引数の安全な拒否**: `env` は引数を取らない。`Command::Env { arguments }` に非空・非空白の引数がある場合は editor を開かず safe な notice を出す（usage は「env」）。空/空白のみは従来どおり editor を開く。
- **ドキュメント更新**: `document/03-tui.md`（Overview / overlay の env 記述）等、実装済みの挙動だけを反映する。ユーザー向け変更があれば `README.md` も。

## 対象外

- daemon lifecycle protocol / `DispatchToolAction` への環境 action 追加（notes と同格の client 側 state.json 書き込みに留める）。
- notes（`LoadNotes` / `SaveNotes`）の本番配線（env と同じ no-op アームにあるが本 issue では触らない。将来同型で対応）。
- agent/terminal launch の `environment_allowlist`（`domain/agent`, `domain/terminal_launch`）や PTY 注入経路の変更。editor が編集するのは workspace/session の environment 正本であり、launch 注入の意味論変更は含めない。
- `DaemonBackend` を本番ループへ全面移設する #258 設計 PR3 の完遂（本 issue は strangler shell への env 配線に限定）。
- 右ペイン / mouse / layout の変更。

## 受け入れ条件

- 対話 Workspace runtime で Overview から `env` を実行すると、active target（root / session）の environment editor が開き、正本から現在値が読み込まれて表示される。
- add / edit / delete して Save すると `state.json` に実際に永続化され、editor を開き直すと反映される。
- load / save 中は saving/loading が表示され、saving 中の再 Save は新規 effect を出さない（二重送信されない）。
- load / save 失敗時は editor に留まり、入力を保持したまま safe error を表示し、再 Save で回復できる。
- `env` に余分な引数（例 `env foo`）を与えると editor を開かず safe notice になる。空/空白は editor を開く。
- Overview の Action mode / Prompt mode の双方で `env` が同じ本番経路に到達する。
- 本番配線が no-op でないことをテストで固定する（fake port の unit test だけで終わらせない）。`dispatch_controller_effect` 経由（または本番 store bridge 経由）で LoadEnvironment/SaveEnvironment が store を叩き event を還流することを検証する。
- 決定的テストが Action/Prompt 両 mode、workspace root / session target、空・既存データ、追加・更新・削除、失敗・再試行を網羅する。
- coverage 100% を維持し、`#[coverage(off)]` は実 IO ラッパ（合成ルート側）に限定する。

## 検証

- `crates/core`: environment usecase の load/set/remove/save を fake/temp store で（root と session target、空・既存、後方互換読み込み）。
- `crates/tui`: reducer（open → load 還流 → set/remove → save → saved/失敗 → 再試行、saving ガード、余分引数拒否）と overlay render（loading/saving/error/empty）を fake port で。
- 本番配線: `dispatch_controller_effect` の env アーム（または本番 store bridge）を fake store で駆動し、effect → store 呼び出し → `EnvironmentLoaded`/`EnvironmentError` 還流が no-op でないことを固定する。
- docs-only 差分ではないため Rust full gate（fmt / clippy / full test / coverage 100%）＋ Markdown link check を CI で green にする。

## 参考

- 先行例（session effect を本番ループへ接続）: #340。overlay 実装: #244。executor `DaemonBackend`: #314。設計: `.agents/designs/258-controller-runtime-migration.md` §4.3 / §5 PR2。
- 主要コード: `crates/tui/src/presentation/mod.rs`（`dispatch_controller_effect` / `run_workspace_controller`）、`crates/tui/src/usecase/application/controller.rs`（`EnvironmentEditor` / `open_environment` / `submit_overview` / Effect・BackendEvent）、`crates/tui/src/usecase/application/daemon_backend.rs`（`TargetStorePort` / `FakeStore`）、`crates/tui/src/usecase/overview/`（`Command::Env`）、`src/runtime/tui.rs`（合成ルート・`DaemonDecisionCommandPort`）、`crates/core/src/usecase/note.rs` と `crates/core/src/domain/workspace_state/mod.rs` と `crates/core/src/infrastructure/store/state.rs`（永続化の鏡写し元）。
