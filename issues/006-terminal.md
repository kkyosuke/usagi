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

**疑似ターミナル（portable-pty + vt100）を右ペインに埋め込む**方式で実装。`terminal` を実行すると、左ペインの worktree 一覧を表示したまま、右ペインがライブシェルに切り替わる。

- `infrastructure/terminal.rs`：起動するシェルの解決（`$SHELL`、未設定なら `bash` / Windows は `cmd.exe`）。テスト可能な純粋ロジックに限定。
- `infrastructure/pty.rs`：portable-pty で PTY を開き、解決したシェルを指定ディレクトリで spawn。出力をバックグラウンドスレッドで `vt100::Parser` に流し込み画面グリッドを保持。リサイズ・入力書き込み・生存判定を提供（端末 I/O のためカバレッジ計測対象外）。
- `presentation/tui/home/terminal_view.rs`：`vt100::Screen` を 1 行 1 文字列＋カーソル位置の純粋なスナップショット（`TerminalView`）へ変換。テスト済み。
- `presentation/tui/home/terminal_pane.rs`：crossterm の raw モード＋イベントポーリングでキー入力をシェルへ転送し、毎フレーム `TerminalView` を右ペインに描画する描画/入力ループ。`Ctrl-O` でデタッチ（端末 I/O のためカバレッジ計測対象外）。
- `terminal` コマンドは従来どおり `Effect::OpenTerminal` を返し、event loop が選択中 worktree（未選択ならワークスペースルート）を解決して右ペインをターミナルモードに切り替え、`home/mod.rs` の `open_terminal` 経由で埋め込みシェルを起動する。シェルの `exit` または `Ctrl-O` で右ペインがコマンド履歴/出力へ戻る。
