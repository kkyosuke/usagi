---
number: 157
title: feat(tui): note オーバーレイを 3 タブ化し TODO 編集・意思決定ログ表示を追加
status: todo
priority: medium
labels: [feat, tui]
dependson: [155]
related: [155]
created_at: 2026-07-09T23:13:50.577305+00:00
updated_at: 2026-07-09T23:13:50.577305+00:00
---

## 背景

#155 でセッションスクラッチパッドの 3 区画（`note` / `todos` / `decisions`）を**データモデル・usecase・MCP**として実装した（`SessionRecord.todos` / `decisions`、ルート版 `root_todos` / `root_decisions`、usecase `add_todo`/`set_todo_done`/`edit_todo`/`remove_todo`/`clear_todos`/`log_decision`/`clear_decisions`、MCP `session_todo_*` / `session_decision_*`）。AI（MCP 経由）はすでに TODO 管理と意思決定ログの追記ができる。

本 issue は残りの **TUI（人間向けの表示・編集）** を実装する。#155 に依存。

## 変更方針

現状の note オーバーレイ（`state/modal.rs` の `NoteEditor`、描画 `ui/panes.rs::note_box`/`note_overlay`、キー処理 `event/handlers.rs::note_editor_key`、起動 `pane_input.rs` の `OpenNote`、配線 `home/mod.rs`）を **3 タブ**に拡張する。

- `Tab` / `Shift-Tab` でタブ（`note` / `todos` / `decisions`）を切替。タイトルに現在タブを表示。
- **note タブ**: 現状の `TextArea`（`Ctrl-S` 保存）を維持。
- **todos タブ**: 選択リスト。`j`/`k` 移動 / `Space` トグル / `a` 追加 / `e` 編集（インライン 1 行入力）/ `d` 削除。usecase の `add_todo` 等へ配線。
- **decisions タブ**: 時刻付きの読み取り専用リスト（AI が MCP で書いたログを人が確認）。
- ルート行（`⌂ root`）でも同じオーバーレイ（`root_todos` / `root_decisions` を対象）。

## ドキュメント

- `document/design/home/05-overlays.md` — メモ編集オーバーレイをタブ構成に更新。
- `document/data/02-workspace.md` — `todos`/`decisions` の編集経路の記述に TUI 操作を追記（現状は MCP 経由のみ記載）。

## テスト・確認方法

- 既存 note の TUI テスト（`state/tests/note_editor.rs`、`event/tests/notes.rs`）を拡張し、タブ切替・todo 操作・decisions 表示を網羅。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo llvm-cov`（カバレッジ 100%）。
