---
number: 006
feature: terminal
title: terminal コマンド（対話型ターミナル）
status: todo
priority: medium
category: tui
dependson: [002]
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
