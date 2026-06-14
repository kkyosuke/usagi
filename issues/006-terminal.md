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
- `infrastructure/pty.rs`：portable-pty で PTY を開き、解決したシェルを指定ディレクトリで spawn。出力をバックグラウンドスレッドで `vt100::Parser` に流し込み画面グリッドを保持。リサイズ・入力書き込み・生存判定に加え、出力を解析するたびに増える**世代カウンタ（`generation`）**を提供し、描画ループが新しい出力に即応できるようにする（端末 I/O のためカバレッジ計測対象外）。
- `presentation/tui/home/terminal_view.rs`：`vt100::Screen` を 1 行 1 文字列＋カーソル位置の純粋なスナップショット（`TerminalView`）へ変換。各セルの**色（前景/背景・名前付き/256/RGB）と装飾（太字・淡色・イタリック・下線・反転）を ANSI（SGR）エスケープとして保持**し、同一スタイルの連続セルはまとめ、既定スタイルはエスケープを出さず、行末でリセットして色漏れを防ぐ。テスト済み。
- `presentation/tui/home/terminal_pane.rs`：crossterm の raw モードでキー入力をシェルへ転送する描画/入力ループ。**キー入力か新しい出力（世代カウンタ）のどちらかで即座に起き**、**前フレームから変化した行だけ**を 1 回の書き込みでまとめて再描画する（ちらつき・遅延を抑制）。`Ctrl-O` でデタッチ（端末 I/O のためカバレッジ計測対象外）。
- `terminal` コマンドは従来どおり `Effect::OpenTerminal` を返し、event loop が選択中 worktree（先頭のルート行を選んでいればワークスペースルート。[031](031-root-mode.md) 参照）を解決して右ペインをターミナルモードに切り替え、`home/mod.rs` の `open_terminal` 経由で埋め込みシェルを起動する。シェルの `exit` または `Ctrl-O` で右ペインがコマンド履歴/出力へ戻る。
- `presentation/tui/screen.rs`：TUI を表示する代替スクリーン中は `AlternateScreenGuard` でマウスレポート（DECSET 1000 + SGR 1006）を有効化し、ホイール操作を端末側のスクロールや矢印キー合成ではなくマウスイベントとして受け取らせる（代替スクロールモード DECSET 1007 の無効化も併用するが、Apple Terminal.app など 1007 を無視する端末があるため確実なのはマウスレポート有効化のほう）。TUI 終了時にいずれも元へ復帰する。
- `presentation/tui/term_reader.rs`：マウスレポート有効化で stdin に流れ込むマウスイベント列（SGR `ESC [ < … M|m` / X10 `ESC [ M Cb Cx Cy`）を `console` がデコードできず後続バイトが文字キーとして漏れるため、`next_non_mouse_key` がこれらの列をまとめて破棄し、次の実キーだけを返す。これによりホイールでメニュー/リストが動いたり埋め込みシェルへ転送されたりしない。フィルタ本体は純粋関数として単体テスト済み。
