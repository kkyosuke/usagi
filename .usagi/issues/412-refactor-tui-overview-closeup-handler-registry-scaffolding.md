---
number: 412
title: refactor(tui): overview/closeup の未使用 handler 層と registry scaffolding を整理する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:54:57.973162+00:00
updated_at: 2026-07-20T11:54:57.973162+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- 本番の実行経路は `controller.rs:2688-2689` → `submit_overview`（:2941）／`submit_closeup`（:3007）で、`overview::interpret` / `closeup::interpret` の返す `Command` を**直接 match** する。
- `crates/tui/src/usecase/overview/mod.rs` の `dispatch`（:349）・`into_handler`（:264）、`closeup/mod.rs` の `dispatch`（:197）・`into_handler`（:112）、各 `commands/mod.rs:18` の `render`、`CommandResult::NotImplemented` は `#[cfg(test)]` からのみ参照（`render()` は呼び出しゼロ）。
- スタブ 8 ファイル: `overview/commands/{config,env,issue,session}.rs`＋`closeup/commands/{agent,close,diff,terminal}.rs` は `CommandResult::not_implemented(...)` を返すだけ。
- registry scaffolding（`trait CommandRegistry` / `struct DefaultRegistry`）が `overview/mod.rs:129,136` と `closeup/mod.rs` にコピペで並存。

## 問題

本番で使われない handler/dispatch 層とスタブがテスト・カバレッジのコストを発生させ、コマンド追加時に「どちらの経路に足すのか」を誤らせる。

## 改善案（要検討）

- 使われる経路（`interpret` → controller 直接 match）だけ残し、`dispatch`/`into_handler`/`render`/`CommandResult::NotImplemented` と handler スタブ 8 ファイルを削除する。
- registry scaffolding は generic 化して 1 実装に統合する（subcommand SSoT 化 issue とも関連）。

## 受け入れ条件

- [ ] テスト以外から呼ばれない handler 層・スタブが削除（または本番配線）されている。
- [ ] registry scaffolding のコピペが解消されている。
- [ ] 既存のコマンド実行の挙動（controller test）が回帰しない。coverage 100% を維持。
