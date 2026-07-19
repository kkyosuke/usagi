---
number: 357
title: fix(tui): New Project 画面で有効な入力のとき Enter で作成を実行する
status: done
priority: high
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-19T11:10:50.105319+00:00
updated_at: 2026-07-19T12:06:23.206525+00:00
---

## 背景 / 問題

TUI の New Project（新規 workspace 作成）画面はフッタで `Enter: create` を案内しているが、実際には
`Enter` が何もしない。実端末経路の入力ルータ `step_new`（`crates/tui/src/presentation/mod.rs`）で
`Key::Enter` が catch-all の no-op（`NewStep::Stay`）に落ちており、作成へ進む遷移が存在しない
（`NewStep` に `Create`/`Submit` 相当のバリアントが無い）。

作成のためのロジックは controller runtime 側に一式（`NewState` / `update_new` / `validate_new_form` /
`NewRequest` / `Effect::CloneProject` / `Effect::RegisterWorkspace` / `NewProjectPort`）揃っているが、
実端末経路の screen graph からは一度も呼ばれておらず、`Effect::CloneProject` / `Effect::RegisterWorkspace`
は各所で no-op として受理されるだけになっている。つまり「Enter で作成」はどの経路でも成立していない。

## ゴール

New Project 画面で、必要な入力が有効なら `Enter` で作成を実行できるようにする。既存の
ボタン/フォーカス操作・validation・`Ctrl+C`/`Esc` 等の契約は壊さず、二重 submit も防ぐ。

## スコープと方針（Approach A: 実端末経路への局所配線）

- 実端末経路（`step_new` / screen graph）を正とし、`Key::Enter` を作成へ配線する。
  - validation は controller の `validate_new_form` を再利用（重複実装しない = SSoT）。`New::to_request`
    が presentation フォームを controller の `NewForm` へ写し、検証済み `NewRequest` を返す。
  - 無効入力 → フィールド別の安全なメッセージを notice に出して同画面に留まる（既存の validation 契約）。
  - 有効入力 → 検証済み `NewRequest` を載せた新しい `NewStep::Create(_)` を返す。
- 作成能力を `WorkspaceLoader::create_workspace` として port に追加し、合成ルートの `FsWorkspaceLoader`
  で実装する。
  - **Clone**: core に `git::clone`（`GitRunner` seam・`FakeGit` で単体テスト可能）を追加し、
    clone 後に既存の open 経路で snapshot を得る。
  - **Existing**: core に `workspace::register(path, name)`（明示 name を衝突回避して登録）を追加し、
    それを通す。name は現状パス末尾から導出済みだが、手編集した name も尊重する。
  - 成功時は Open / Recent と同じ `open_snapshot_via_controller` へ合流し、Home へ遷移する。
  - 失敗時は入力中の draft を保持したまま notice を出して同画面に留まる（仕様どおり）。
- 二重 submit 防止:
  - 実端末経路は同期ループ（`read_key` でブロック）なので、1 回の `Enter` で 2 回作成は構造的に起きない。
  - controller reducer は既存の `pending` トークンで late/duplicate submit を弾く。これを回帰テストで固定する。
- `Ctrl+C`（`Key::Quit`）/`Esc`（`Key::Escape`）/Tab 補完/←→/↑↓ 等の既存キー契約は不変のまま。

## テスト（回帰）

- controller runtime: `update_new` の `Submit` が有効入力で `Effect` を出す / 無効入力で notice のみ /
  `pending` 中の 2 回目 `Submit`（および `Retry`）を弾く。
- 実端末経路（entry runtime）: `step_new` の Enter が「無効 → Stay + notice」「有効 → Create」を返す。
  screen graph 全体を注入 terminal（`FakeTerminal`）＋注入 loader（`FakeLoader`）で回し、Enter が
  Clone / Existing の作成を 1 回だけ呼んで Home へ遷移すること、作成失敗で draft を保ったまま notice を
  出して同画面に留まることを固定する。これは実端末経路の実コード（`step_new` → `Screen::New` handler →
  `WorkspaceLoader`）をそのまま通す。
- core: `git::clone`（引数・branch・失敗時 stderr）と `workspace::register`（明示 name・衝突回避・
  空名フォールバック・既存 path の再利用）。
- New view: `New::to_request` の Clone / Existing / 必須欠落、および `new_project_notice` の 1 行化。

## ドキュメント

- `document/03-tui.md` の New 画面（Welcome の New）記述を、Enter による作成確定・成功時 Home 遷移・
  失敗時 draft 保持・二重 submit 防止の実装済み挙動に更新する。

## 受け入れ条件

- New Project 画面で有効入力のとき `Enter` で workspace が作成され Home へ遷移する。
- 無効入力のとき `Enter` は notice を出して同画面に留まる。
- `Esc` で Welcome へ戻り、`Ctrl+C` で終了する挙動が変わらない。
- 二重 submit が発生しない。
- 上記テストが通り、coverage 100% と規約 gate を満たす。
