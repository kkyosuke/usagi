---
number: 56
title: refactor(tui-home): HomeState の神オブジェクト解体（表示文字列の ui 退避・サブ状態の型化・dispatch 重複統合）
status: done
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-06-19T22:15:51.176691+00:00
updated_at: 2026-07-04T00:14:18.219688+00:00
---

## 背景

`src/presentation/tui/home/state/mod.rs`（1324 行）が肥大し、責務漏れも抱えている。規約の「1 ファイル 300 行目安」「state は presentation 非依存」を逸脱。

### 1. 表示文字列の組み立てが state に漏れている（責務違反・高）
`log_sessions`（`:335-354`）・`hint_no_live_session`（`:415-419`）・モーダル本文構築が、絵文字・英文込みのユーザー向け文字列（`"No sessions yet. Run \"session create <name>\"..."`、`"{} session(s):"` 等）を「純粋・presentation 非依存」を謳う state 層で生成している。→ 構造化データを返し、整形は `ui/` に任せる。

### 2. サブ状態機械が平坦な forwarding メソッドの羅列
create 入力 / rename / focus-menu カーソル / focus-prompt 編集 / remove-modal の各サブ状態が `HomeState` に約 80 個の薄い委譲メソッド（`create_cursor_left` → `input.move_left` 等）として同居（`:723-902`, `:1212-1288`）。→ 各サブモードを独自型に切り出し、自前のメソッドを公開させる。

### 3. dispatch+history+modal ロジックの二重化（潜在バグ）
`focus_prompt_submit`（`:1044-1068`）と `submit`（`:1156-1197`）が trim→空 early-return→`dispatch_with`→history push→`Effect::ShowText` 分岐をそれぞれ実装。`focus_prompt_submit` は `response_start` を設定しないため、後で Overview に切り替えるとログ表示が不整合になりうる。→ `dispatch_and_record(entry) -> CommandResult` 共通ヘルパへ集約。

## 確認方法

- 表示文字列が ui 層に移り、state のユニットテストが文字列に依存しなくなること。
- focus-prompt 経由・通常 submit 経由の双方でログ/モーダル挙動が一致すること。
- 既存テストが通ること（カバレッジ 100% 維持）。
