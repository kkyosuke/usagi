---
number: 360
title: pre-push の重い full gate（workspace clippy / coverage 100%）を撤去し PR CI に一本化する
status: done
priority: medium
labels: [chore]
dependson: []
related: []
created_at: 2026-07-19T19:51:07.157341+00:00
updated_at: 2026-07-19T19:53:54.732564+00:00
---

## 背景 / 目的

lefthook の `pre-push` は workspace 全体の `cargo clippy` とカバレッジ 100%（`cargo llvm-cov`。テスト実行を兼ねる）を実行しており、push のたびにローカルで重い full gate が走る。これが push の完了を遅くし、開発のリズムを損なっている。

品質の最終保証は既に PR CI（`test.yml` の fmt / clippy / full test、`coverage.yml` の coverage 100%）で二重化されているため、**ローカル pre-push の重い gate は撤去し、full gate を PR CI に一本化**する。pre-push を軽くする代わりに、開発中は fast feedback（fmt check / `cargo check` / 変更箇所の selected tests）を回し、PR は Draft で開いて CI 成功後に Ready for review とする運用へ寄せる。

## スコープ / 変更内容

- `lefthook.yml`
  - `pre-push` セクションの `clippy` / `coverage` コマンドを撤去する（セクションごと削除、または実態に合わせて整理）。
  - 冒頭コメントの「push 時に clippy とカバレッジを確認」の記述を実態に合わせて更新する。
  - `pre-commit`（workspace-root guard / branch-name / 差分 fmt）と `commit-msg`（Conventional Commits）は**維持**する。
- `document/06-conventions.md`
  - 「品質チェック（リスク比例の gate）」表・本文から「push / PR 前」のローカル full gate 強制を外し、full gate は PR CI が担う旨に更新する。
  - 「Git Hooks（lefthook）」表の pre-push 行を実態（重い gate なし）に更新する。
  - PR を Draft で開き CI 成功後に Ready for review とする運用を追記する。
- `.agents/workflow.md`
  - 「pre-push でチェックされる」等の記述を実態に合わせ、CI に一本化された旨・Draft PR 運用に更新する。
  - 06-conventions.md と矛盾がないよう SSoT（正本は 06-conventions.md）を保つ。

## 非スコープ / 制約

- **CI の必須チェック自体は弱めない**（`test.yml` / `coverage.yml` / `markdown-link-check.yml` / `enforce-pr-base.yml` はそのまま）。
- pre-commit / commit-msg フックは変更しない。

## テスト・確認方法

- `lefthook.yml` の YAML 妥当性を確認する（`lefthook validate` などが使える場合）。
- ドキュメントのリンク/アンカー整合を確認する（`lychee`）。
- docs / 設定変更のみで Rust 差分が無いことを確認し、Rust full gate の省略可否を完了報告に記載する。

## 完了条件

- 上記 3 ファイルが互いに矛盾なく更新されている。
- pre-push で重い gate が走らないことがフック定義から読み取れる。
- CI 必須チェックは従来どおり。
