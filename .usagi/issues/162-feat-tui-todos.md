---
number: 162
title: feat(tui): todos タブを対話編集可能にする（トグル / 追加 / 編集 / 削除）
status: todo
priority: low
labels: [feat, tui]
dependson: [157]
related: [155, 157]
created_at: 2026-07-10T00:25:36.220253+00:00
updated_at: 2026-07-10T00:25:36.220253+00:00
---

## 背景

#157 でメモオーバーレイを 3 タブ化し、`todos` / `decisions` を**読み取り表示**にした。本 issue は `todos` タブを**対話編集**できるようにする。

## 変更方針

- `todos` タブのキー操作: `j`/`k` 選択移動 / `Space` 完了トグル / `a` 追加（インライン 1 行入力）/ `e` 編集 / `d` 削除。
- 永続化: エディタのスナップショットを in-memory で編集し、`Wiring` に 1 本の保存クロージャ（例 `set_todos(root, name, &[SessionTodo]) -> SessionOutcome`）を追加して反映。usecase 側に「チェックリスト全体を置換する」`set_todos` を追加するか、既存の `add_todo`/`set_todo_done`/`edit_todo`/`remove_todo` を配線する。
- `NoteEditor` に選択インデックスとインライン入力（`TextInput`）状態を追加。
- ルート行の `root_todos` も編集対象にするなら、`HomeState` に `root_todos` を復元する配線が要る（現状 TUI スナップショットはセッションのみ）。

### 注意

- `Wiring` は 10 箇所前後で構築され、`event_loop_compat` / `run_notes` にも簡易版シグネチャがある。新クロージャはこれら全てに波及するため、機械的だが広い変更になる。
- カバレッジ 100%（特にエラーパスのクロージャ）に注意（[[coverage-workspace-table-underreports]] 参照の勘所）。

## テスト・確認

- state / event / ui にトグル・追加・編集・削除・インライン入力のテストを追加。
- `cargo fmt` / `clippy` / `llvm-cov`（lines・functions 100%）。
