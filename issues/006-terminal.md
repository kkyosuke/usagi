---
number: 006
feature: terminal
title: terminal コマンド（対話型ターミナル）
status: done
priority: medium
category: tui
dependson: [002, 003]
ref: usagi.ai doc/app/tui/terminal.md
---

# `terminal` コマンド（対話型ターミナル）

## 概要

アクティブなワークスペース配下で対話型シェル（bash / cmd.exe）を起動する TUI 内コマンドを実装します。AI に任せきれない手作業や確認を、TUI から離れずに行えるようにします。

## やること

- `terminal` でアクティブ worktree をカレントディレクトリとした対話型シェルを起動する。
- 疑似ターミナル（portable-pty + vt100）で TUI 内にシェルを埋め込む、または一時的に TUI を抜けてシェルに入り、終了後に復帰する。
- OS に応じて `bash`（macOS/Linux）/ `cmd.exe`（Windows）を選択する。

## 完了条件

- `terminal` でアクティブ worktree 配下のシェルが起動する。
- シェル終了後にワークスペース画面へ正しく復帰する。

## 実装状況

「一時的に TUI を抜けてシェルに入り、終了後に復帰する」方式で実装（PTY 埋め込みは将来課題）。

- `infrastructure/terminal.rs`：`$SHELL`（未設定時は `bash` / Windows は `cmd.exe`）を指定ディレクトリで起動。シェルの終了コードは無視し、起動失敗のみエラーにする。
- ワークスペース画面の `terminal` コマンドは、サイドバーで選択中の worktree（未選択ならワークスペースルート）を作業ディレクトリにする。実行時は alternate screen を一旦抜けてシェルに入り、終了後に復帰する（`presentation/tui/home/mod.rs`）。
