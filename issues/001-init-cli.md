---
number: 001
feature: init-cli
title: usagi init <URL> CLI コマンド
status: todo
priority: high
category: cli
dependson: []
ref: usagi.ai doc/cli/init.md
---

# usagi init `<URL>`

## 概要

リポジトリをクローンし、作業ディレクトリを初期化する CLI コマンドを追加します。
現状 `main.rs` のサブコマンドは `doctor` / `hop` / `status` のみで、`overview.md` に記載のある `usagi init <URL>` が未実装です。TUI の New 画面（#016 既存ディレクトリ登録 / clone 作成）と同等の初期化ロジックを、ターミナルから一発で実行できるようにします。

## やること

- `usagi init <repository-url>` サブコマンドを `clap` に追加する。
- 指定 URL を `main/` にクローンする。
- `.usagi/state.json`（初期化フラグ・worktree 一覧）と `usagi.config`（リポジトリ URL 等）を生成する。
- `.gitignore` に `.usagi/` を追記する。
- グローバルレジストリ（`repositories.json`）に初期化済みプロジェクトを登録する。

## 完了条件

- 空ディレクトリで `usagi init <URL>` を実行すると `main/` / `.usagi/` / `usagi.config` / `.gitignore` が作成される。
- 既に初期化済みのディレクトリではエラーまたは警告を表示する。
- 既存の `usecase`（初期化フロー）・`infrastructure`（git, storage, workspace_store）を再利用し、TUI New 画面とロジックを共有する。
