---
number: 027
feature: agent-wait-notify
title: 埋め込みターミナルのバックグラウンド常駐と入力待ち通知
status: done
priority: medium
category: tui
dependson: [006, 026]
---

# 埋め込みターミナルのバックグラウンド常駐と入力待ち通知

## 概要

`terminal` / `agent` で開いた埋め込みターミナルを、デタッチ後もバックグラウンドで動かし続けられるようにし、`claude` などの Agent が処理を終えてユーザーの入力を待つ状態になったことを、**サイドバーのマーカー**と**デスクトップ通知**で知らせます。複数セッションを並行で走らせ、入力が必要になったものだけに対応する、という使い方を可能にします。

## やること

- `terminal` / `agent` のセッションを worktree ごとに常駐保持し、`Ctrl-O` デタッチ後も動作継続（同じ worktree で再実行すると再アタッチ。再起動しない）。
- Agent の入力待ちを **ターミナルベル（`^G`）** で検知する。
- アタッチ中でないセッションが新たに入力待ちになったら、左ペインの該当 worktree に `◆`（黄色）マーカーを付け、デスクトップ通知（`🐰 <ブランチ名> が入力待ちです`）を 1 回出す（設定 `notifications_enabled` 尊重）。
- 該当 worktree に再アタッチするとマーカーを消す。シェル / Agent が `exit` したセッションは破棄する。

## 完了条件

- デタッチ後もバックグラウンドの `claude` が動作を続け、再アタッチで続きを操作できる。
- バックグラウンドの Agent が入力待ちになると、サイドバーにマーカーが付き、デスクトップ通知が出る。
- アタッチ中のセッション自身は（ユーザーが見ているため）マーカー・通知の対象にしない。

## 実装状況

- `infrastructure/pty.rs`：vt100 の `Callbacks::audible_bell` でターミナルベルの回数を計測し `bell_count()` / 監視用ハンドルを公開（端末 I/O のためカバレッジ計測対象外）。
- `infrastructure/session_monitor.rs`：入力待ち判定の**純粋ロジック**（`SessionMonitor`）。各セッションのベル基準値・入力待ち集合・アタッチ中セッションの扱いを管理し、「新たに入力待ちになったもの」を返す。テスト済み（lines / functions / regions 100%）。
- `infrastructure/terminal_manager.rs`：`TerminalManager` が worktree パスごとに `PtySession` を常駐保持。約 200ms 間隔の監視スレッドが各セッションのベルを `SessionMonitor` に渡し、入力待ちになったセッションのデスクトップ通知を発火。終了済みセッションは prune。`MonitorHandle` で入力待ち集合とアタッチ状態を presentation 層へ公開（端末 I/O・スレッドのためカバレッジ計測対象外）。
- `presentation/tui/home/`：`HomeState` に入力待ちパス集合（`set_waiting` / `waiting_paths`）、`ui.rs` のサイドバー行に `◆` マーカー、`event.rs`／`terminal_pane.rs` の各描画ループで監視ハンドルから入力待ち集合を反映。`home/mod.rs` が `TerminalManager` を所有し、`open_terminal` で再アタッチ／常駐とアタッチ状態の宣言を行う。

## 既知の制約

- 検知はターミナルベルに依存するため、Agent 側でベル通知を有効にしておく必要がある（鳴らさない CLI では入力待ちマーカー・通知は出ない）。
- ホーム画面（サイドバー）でアイドル中のマーカー更新はキー操作のたびに反映される（通知はベル検知の時点で即時に出る）。アタッチ中は定期再描画でライブ更新される。
