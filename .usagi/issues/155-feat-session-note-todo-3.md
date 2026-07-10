---
number: 155
title: feat(session): note を「メモ / TODO / 意思決定ログ」の 3 区画スクラッチパッドに拡張
status: done
priority: medium
labels: [feat, session, tui, mcp]
dependson: []
related: [157]
created_at: 2026-07-09T22:55:17.749077+00:00
updated_at: 2026-07-09T23:21:14.960266+00:00
---

## 背景 / 目的

現状の note はセッション（および `⌂ root`）に紐づく **1 本のフリーテキスト**（`SessionRecord.note` / `WorkspaceState.root_note`、`state.json`・git 管理外・表示専用）。すでに「次回 TODO の確認」用途で使われており、暗黙に TODO 置き場になっている。

これを、セッション作業の**スクラッチパッド**として次の 3 区画に正式化する:

1. **メモ (`note`)** — 従来どおりの自由記述（経緯・リンク・覚え書き）。
2. **TODO (`todos`)** — そのセッション内の軽量チェックリスト（`[ ]`/`[x]`）。人も AI も編集する。
3. **意思決定ログ (`decisions`)** — AI が「なぜその方針にしたか」を時刻付きで**追記**する append-only ログ。root（コーディネータ）が transcript 全体を読まずに判断根拠を追える。

### スコープ（この issue = ベース: データ + usecase + MCP + docs）

**AI 向けの能力を丸ごと**実装する。TUI（人間向けの表示・編集）は分量・リスクが大きいため follow-up #157（本 issue に依存）に切り出す。

- **issue ストア (`.usagi/issues/`) とは別物**。あちらは git 管理の正式タスク。ここの TODO は起票するほどでない**セッション内の使い捨てチェックリスト**で、note と同じ**マシンローカル**側に置く。
- 保存先は従来どおり `state.json`（git 管理外）。単一マシン上の worktree 運用なので root からも同じ state.json を読め、共有問題は起きない。

## データモデル（domain: `workspace_state.rs`）✅

- `SessionTodo { text: String, done: bool }`（`done=false` はファイル省略）
- `SessionDecision { at: DateTime<Utc>, text: String }`（`at`=RFC3339 UTC）
- `SessionRecord` に `todos` / `decisions`、`WorkspaceState` に `root_todos` / `root_decisions`。いずれも `#[serde(default, skip_serializing_if = "Vec::is_empty")]` で後方互換。

## usecase（`usecase/session/mod.rs`）✅

`NoteTarget { Root, Session(&str) }` を導入し、`note` と同じロック運用の read-modify-write で:

- TODO: `add_todo` / `set_todo_done` / `edit_todo` / `remove_todo` / `clear_todos` / `get_todos`
- 意思決定: `log_decision`（`at` は合成ルートから注入）/ `get_decisions` / `clear_decisions`

`text` は trim・空を拒否。index は範囲外を明示エラー（save 前に短絡）。

## MCP（`presentation/mcp/session.rs`、セッション worktree 内限定）✅

`session_todo_list` / `session_todo_add` / `session_todo_update`（`done` と/または `text`）/ `session_todo_remove` / `session_decision_list` / `session_decision_log`（`at` はサーバが付与）。統合 `usagi` サーバの tool 数は 20→26。

## ドキュメント ✅

- `document/data/02-workspace.md` — フィールド表・例・`SessionTodo`/`SessionDecision` サブ表・編集経路。
- `document/03-commands/03-mcp.md` — 追加 MCP ツール（欠落していた `session_note_*` 行も補完）とスクラッチパッドの説明。

## follow-up

- **#157** — TUI（note オーバーレイの 3 タブ化・TODO 編集・意思決定ログ表示）。本 issue に依存。

## テスト・確認方法

- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo llvm-cov`（カバレッジ 100%）。
- serde round-trip（旧ファイル互換・空省略）、usecase の各操作（trim/範囲外/クリア/root 対称）、MCP ハンドラ（round-trip・不正入力・セッション外エラー）。
