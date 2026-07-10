---
number: 157
title: feat(tui): note オーバーレイを 3 タブ化し todos/decisions を表示
status: done
priority: medium
labels: [feat, tui]
dependson: [155]
related: [155, 162]
created_at: 2026-07-09T23:13:50.577305+00:00
updated_at: 2026-07-10T00:25:50.641877+00:00
---

## 背景

#155 でセッションスクラッチパッドの 3 区画（`note` / `todos` / `decisions`）を**データモデル・usecase・MCP**として実装済み。本 issue はその **TUI 表示**を実装した。

## 実装した内容（この PR）

home 画面のメモオーバーレイ（選択 `n` / 集中・没入 `Ctrl-E`）を **3 タブ**に拡張:

- `Tab` / `Shift-Tab` でタブ循環（`note` → `todos` → `decisions`）。タイトル＝現在タブ、フッターのヒントもタブ連動。
- **note タブ**: 従来どおり編集可能（`TextArea`、`Ctrl-S` 保存、選択・キャレット）。
- **todos タブ**: `[x]`/`[ ]` のチェックリストを**読み取り表示**（未登録は `(todo なし)`）。
- **decisions タブ**: `MM-DD HH:MM  内容` の意思決定ログを**読み取り表示**（未記録は `(記録なし)`）。
- ルート行でも同じオーバーレイ（`note`＝`root_note` 編集。todos/decisions タブは表示）。

### 実装メモ

- 状態: `NoteTab` enum と `NoteEditor` に `tab` / `todos` / `decisions` スナップショット（開いた時に session から複製）。`note_editor_cycle_tab`。
- 描画: `ui/panes.rs::note_box` に `title` / `active` を追加、`note_overlay` がタブ別に本文を組む。`todo_lines` / `decision_lines`。フッターは `ui/chrome.rs`。
- キー: `event/handlers.rs::note_editor_key` に `Tab`/`BackTab` を追加、編集キーは note タブのみ有効（todos/decisions は読み取り専用）。

## スコープ外（follow-up #162）

TUI からの**対話的な TODO 編集**（`Space` トグル / `a` 追加 / `e` 編集 / `d` 削除）は、`Wiring`／`event_loop_compat` に永続化クロージャを通す必要があり分量が大きいため別 issue（#162）に切り出す。現状 TODO の編集は AI が MCP で行う。

## テスト・確認

- state（タブ循環・スナップショット・root 空）、event（`Tab`/`BackTab` 切替・読み取りタブで編集キー無効）、ui（todos/decisions 描画・空プレースホルダ）を追加。
- `cargo fmt` / `clippy` / `llvm-cov`（lines・functions 100%）。
