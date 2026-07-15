---
number: 67
title: chore(tui): coming-soon プレースホルダコマンドを規約に沿って整理する
status: done
priority: medium
labels: [chore, tui, review]
dependson: []
related: []
created_at: 2026-06-20T12:04:27.662869+00:00
updated_at: 2026-06-20T12:04:27.662869+00:00
---

## 背景

コードレビューで判明したドキュメント規約違反。`src/presentation/tui/home/command/builtins.rs:551-590` の `ComingSoonCommand` が `ai` / `doctor` を「"{name}" is coming soon 🐰」と表示するためだけに存在し（登録元 `src/presentation/tui/home/command/registry.rs:23-54`）、`man` にも未実装コマンドとして並ぶ。`src/presentation/tui/home/event/handlers.rs:475` 付近の `run_focus_command` にも coming-soon 文言がハードコードで重複している。

これは CLAUDE.md / `document/06-conventions.md`「記載＝実装済み（未実装機能・"coming soon" を置かない）」に正面から反する。しかも `doctor` は usecase 層に実装が存在する（`src/usecase/doctor/`、`src/usecase/local_llm.rs`）のに、TUI からは placeholder のまま。実装状況の SSoT が崩れている。

## 改善方針

- 未実装コマンド（`ai` 等）はレジストリ・`man` から外し、ロードマップは issue ストア（`.usagi/issues/`）で管理する（規約通り）。
- `doctor` は実装済みなので、placeholder をやめて実際の usecase に配線するか、TUI から提供しない方針なら登録自体を外す（どちらにするかは別途判断）。
- builtins と handlers に重複する coming-soon 文言を解消する。

## 確認方法

- `man` / コマンド一覧に未実装の placeholder が出ないこと。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
