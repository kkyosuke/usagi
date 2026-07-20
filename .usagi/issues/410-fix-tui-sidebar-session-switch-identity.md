---
number: 410
title: fix(tui): sidebar session 行のダブルクリック switch を identity ベースに戻す
status: done
priority: high
labels: [tui, bug, controller, input]
dependson: []
related: [318, 393]
created_at: 2026-07-20T06:35:08.485476+00:00
updated_at: 2026-07-20T06:35:16.009001+00:00
---

## 背景 / 症状

TUI の左 sidebar で session 行を左ダブルクリックすると、その session を選択・active にして Closeup へ切り替わる（keyboard の Enter 相当）はずだが、この switch が実質機能していない回帰が出ている。

## 原因

controller-driven Home 移行後、ダブルクリック判定は composition shell（`crates/tui/src/presentation/mod.rs` の `drive_workspace_controller` 内 `sidebar_pointer_event`）にあり、**clicked cell の生座標 `(column, row)`** をキーにしていた。

- session 行は 2 行（name 行＋補足行）にまたがるため、実際のダブルクリックが 1 セルでもずれる／2 回目が同じ行の別ラインに落ちると、別セル扱いで 2 回の single click になり **switch が発火しない**。
- 逆に scroll や snapshot 更新で 2 回目までに cursor 下の行が別 session に入れ替わっても、座標が同じなら doubled 判定になり **古い/別の identity を誤 activate** しうる。
- 判定ロジック全体が `#[coverage(off)]` の shell にあり、決定的テストが無い（回帰を検知できなかった）。

## 対応

- ダブルクリック判定を `WorkspaceRuntime`（テスト可能な runtime 層）へ移し、clicked cell ではなく **解決した行 identity（`Selection`、selected と同じ hit-test）** をキーにする。
- 時間窓（400ms）は `now: Instant` を注入して決定的にする（shell は `Instant::now()` を渡す）。
- pointer 活性化の対象は実 session 行のみ。root・`+ new session` は cursor 移動のみで pointer では activate しない（作成フォームは Enter / `Ctrl-A` のまま）。overlay / inline 作成中と sidebar body 外の click は inert にし、pending gesture をリセットする。
- shell の `sidebar_pointer_event` / `SIDEBAR_DOUBLE_CLICK` / `last_click` は撤去し、`runtime.pointer_click(column, row, now)` に一本化。
- `document/03-tui.md` の click 契約を identity ベースの挙動へ更新。

## 受け入れ条件（決定的 reducer/runtime テスト）

- 単クリック＝選択のみ（Switch のまま、active は root）。
- 同一 session 行の正常なダブルクリック＝Closeup へ切替。
- 同じ行内のセルジッター（別ライン/別カラム）でもダブルクリックが成立。
- 時間切れ（>400ms）は 2 回の single click。
- 別行での 2 回目は activate しない。
- root / `+ new session` は pointer ダブルクリックで activate しない。
- snapshot 更新（同名再作成＝新 identity）で cursor 下が入れ替わったら activate しない。
- scroll で同じセルが別行になったら activate しない。
- modal / inline input 表示中の背景クリックは inert。
- sidebar body 外クリックは pending gesture をリセット。

## 確認

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p usagi-tui` / `cargo test -p usagi`
- full test / coverage は PR CI。
