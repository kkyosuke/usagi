---
number: 370
title: fix(tui): 新規 session 作成の入力 validation を明確に表示し daemon 失敗を握り潰さない
status: done
priority: high
labels: [tui, session, bug, validation, ux]
dependson: []
related: [357, 287, 257]
created_at: 2026-07-19T21:18:26.078166+00:00
updated_at: 2026-07-19T21:30:24.859163+00:00
---

## 背景 / 問題

TUI の新規 session 作成は、`document/03-tui.md`（Session sidebar rows）で **name-only の
inline 入力**として仕様化されており、「入力中は英数字・`-`・`_` 以外、64 文字超過、または表示中
session と重複する名前を行の下に error として表示し、空の名前は Enter 時に error を表示する」と
定義されている。しかし実装（`crates/tui`）はこの validation を満たしておらず、利用者に作成失敗が
明確に伝わらない。具体的な gap は次のとおり。

- **local validation が空名だけ**。`CreateSessionForm`（`usecase/application/controller.rs`）の
  `required_create_value` は trim して非空を見るだけで、不正文字・64 文字超過・表示中 session との
  重複を検査しない。仕様が要求する `不正 name` / `同名` は local には一切検出されない。
- **daemon 失敗が握り潰される**。実端末経路（`run_workspace_controller` → 合成ルートの
  legacy sync `DaemonSessionCommandPort` → `begin_session_command` / `drain_session_completions`）は
  `Result<SessionCommandResult, String>` の `Err` を `presentation/mod.rs` の `drain_session_completions`
  で **黙って捨てている**。daemon が作成を拒否（例: 同名・worktree 作成失敗）しても notice が出ず、
  利用者は何が起きたか分からない。port の `Err(String)` は契約上 display-safe（trait doc: "Returns a
  safe message"）なので、表示しても raw/internal detail は漏れない。
- **submit で draft を即破棄**。`update_create_session_form` の `Enter` 成功枝は effect を出す前に
  `create_session = None` にするため、（万一 local を素通りした）daemon 失敗時に入力へ戻れず再送できない。
- **name-only との不整合**。modal は `name` / `profile` / `model` の 3 フィールドを持つが、daemon へ渡る
  のは `SessionCommand::Create { name }` の name だけで、profile/model は死んだ入力になっている
  （名前 only 仕様と矛盾）。

結果として「空白名・不正 name・同名」いずれの validation failure も、利用者に明確・安全には
表示されない。

## ゴール

新規 session 作成時の validation error を、利用者に明確・安全に表示する。

- 空白名・不正 name（`[A-Za-z0-9_-]` 以外）・64 文字超過・表示中 session との同名を **local に検出**し、
  入力を失わずに再編集・再送できるようにする（draft 保持）。
- **daemon failure と local validation error を区別**する。local validation は入力欄付随の error、
  daemon failure は safe な notice として表示し、握り潰さない。
- raw / internal detail は表示しない（daemon の safe message 契約に従う）。

## スコープ（本 issue / PR）

controller を SSoT として validation を強化し、実端末経路の daemon 失敗表示を修正する。inline row への
描画置換（`+ new: <name>` 行・skeleton wave）や `OperationResult` executor への cutover は **別作業**であり
本 PR に混ぜない（下記「対象外 / 追跡」）。

- **local name validation の強化**（controller、pure）:
  - 空名 → **Enter 時のみ** error（入力中は出さない）。
  - `[A-Za-z0-9_-]` 以外の文字 → 入力中に即 error。
  - 64 文字超過 → 入力中に即 error。
  - 表示中 session と重複する名前 → 入力中に即 error。
  - いずれも field 固有の **safe な短い message**。draft は常に保持し、再編集・再送できる。
- 重複検出のため、controller `AppState` が表示中 session の **name** を保持できるようにする
  （現状は `SessionId` のみ）。実端末経路の session 同期でこの name を供給する。
- **daemon 失敗を握り潰さない**（実端末経路の wiring）: `drain_session_completions` の `Err(message)` を
  contractually-safe な `BackendEvent::Notice` として controller へ還流し、利用者に見える形にする。
  daemon failure（notice）と local validation（入力欄付随 error）は視覚的に区別される。
- validation は local に完結するため、空白名・不正 name・同名は daemon へ到達する前に弾かれ、draft は
  自然に保持される。真の daemon failure（想定外の infra error 等）は safe notice で可視化する。

## 対象外 / 追跡（本 PR に混ぜない）

- modal → inline row（`+ new: <name>`）への描画置換、skeleton の wave 表示、`c` / `t` / `Ctrl-A` の
  inline entry 化。
- legacy sync `SessionCommandPort` から `daemon_backend::DaemonBackend`（`AppEvent::OperationResult`
  loop・token 相関）への cutover。cutover 後は daemon failure 時にも form を開いたまま draft を保持し
  再送する UX にできる。本 PR は「daemon failure を握り潰さない」までを最小で満たす。
- modal の profile/model フィールド撤去（name-only 化）。UI 変更のため別 issue。

## テスト（回帰）

- **pure（controller reducer）**: `CreateSessionForm` の validation を、空（Enter 時のみ）/ 不正文字 /
  64 文字超過 / 同名 それぞれで固定。error 時に draft が保たれること、修正すると error が消えて submit で
  effect が出ることを固定。
- **render（modal view）**: 各 error message が modal に描画されること。
- **runtime（実端末経路）**: fake terminal + fake session command port で、daemon が `Err`（safe message）を
  返したとき notice が出て入力が可視に失敗すること（従来は無表示）。local validation error（入力欄）と
  daemon notice が別経路であることを固定。

## ドキュメント

- `document/03-tui.md` の新規 session 作成 validation 記述を、実装した local validation（空 = Enter 時 /
  不正文字 / 64 超過 / 同名）と daemon failure の safe notice 表示に整合させる。inline row 描画は未実装の
  ままなので、実装済みの surface（現行 modal）を偽って inline と書かない。

## 受け入れ条件

- 空白名で Enter → 入力欄に error、draft 保持、再入力で解消。
- 不正文字 / 64 文字超過 / 同名を入力すると即 error、draft 保持、修正で解消。
- 有効名で Enter → 作成 effect が 1 回発行され、作成へ進む。
- daemon が作成を拒否したとき safe notice が表示され、握り潰されない（local validation とは別表示）。
- raw / internal detail を表示しない。
- 上記テストが通り、coverage 100% と規約 gate を満たす。
