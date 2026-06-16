---
number: 28
title: 埋め込みターミナルの入力待ち検知と通知
status: done
priority: medium
labels: [tui]
dependson: [6, 26]
related: []
created_at: 2026-06-16T23:05:07.430225+00:00
updated_at: 2026-06-16T23:08:27.656800+00:00
---

# 埋め込みターミナルの入力待ち検知と通知

## 概要

[issue 006](006-terminal.md) のターミナルプール（[issue で実装済みのバックグラウンド常駐](006-terminal.md)）の上に、`claude` などの Agent が処理を終えてユーザーの入力を待つ状態になったことを **サイドバーのマーカー**と**デスクトップ通知**で知らせる層を追加します。複数セッションを並行で走らせ、入力が必要になったものだけに対応する、という使い方を可能にします。

## やること

- 常駐中（`TerminalPool`）の各セッションについて、Agent の状態を **ライフサイクルフックの phase（正）** と **ターミナルベル（`^G`、フォールバック）** で検知する。
- 各セッションの**表示状態**を `▶ running` / `◆ waiting` / `⏸ idle` のいずれかに定め、左ペイン 2 行目に出す（優先順: アイドル > 入力待ち > 稼働中）。表示状態はフックの phase と常に一致させ、アタッチ中（操作中）のセッションも他と同じバッジを出す（切替プレビューと没入で食い違わせない）。
- バックグラウンド（アタッチ中でない）のセッションが新たに**入力待ち**または**完了**になったら、デスクトップ通知を 1 回出す（入力待ち `🐰 <ブランチ名> が入力待ちです` / 完了 `🐰 <ブランチ名> が完了しました`、設定 `notifications_enabled` 尊重）。
- シェル / Agent が `exit` したセッションは破棄する。

## 完了条件

- バックグラウンドの Agent が入力待ち・完了になると、サイドバーのバッジが変わり、デスクトップ通知が出る。
- アタッチ中（操作中）のセッションも実際の状態バッジを表示する。アタッチが抑制するのは通知とベル推定だけ。

## 実装状況

- `infrastructure/pty.rs`：vt100 の `Callbacks::audible_bell` でターミナルベルの回数を計測し `bell_count()` / 監視用ハンドル（`bell_handle` / `alive_handle`）を公開（端末 I/O のためカバレッジ計測対象外）。
- `infrastructure/session_monitor.rs`：状態判定の**純粋ロジック**（`SessionMonitor`）。各セッションのベル基準値・入力待ち集合・完了集合・アタッチ中セッションの扱いを管理し、「新たに入力待ち／完了になったもの」を種別（`NoticeKind::Waiting` / `Done`）付きで返す。表示状態は phase と常に一致させ、アタッチは通知発火とベル推定のみ抑制する。テスト済み（lines / functions / regions 100%）。
- `presentation/tui/home/terminal_pool.rs`：[issue 006](006-terminal.md) の `TerminalPool`（常駐保持）に監視を統合。約 200ms 間隔の監視スレッドが各セッションの phase / ベルを `SessionMonitor` に渡し、入力待ち・完了になったバックグラウンドセッションのデスクトップ通知を種別ごとに発火。終了済みセッションは prune。`MonitorHandle` で入力待ち集合（`waiting()`）・完了集合（`done()`）・稼働集合（`live()`）とアタッチ状態を描画ループへ公開（端末 I/O・スレッドのためカバレッジ計測対象外）。
- `presentation/tui/home/`：`HomeState` に入力待ち（`set_waiting` / `waiting_paths`）・完了（`set_done` / `done_paths`）の各パス集合、`ui/panes.rs` のサイドバー行に `◆ waiting` / `⏸ idle` マーカー（`AgentState`）、`event/mod.rs`／`terminal_pane.rs` の各描画ループで `MonitorHandle` から各集合を反映。`home/mod.rs` が `open_terminal` でアタッチ中セッションを監視へ宣言（デタッチ／終了で解除）。

## 既知の制約

- フックを持たない Agent（`gemini` など）の検知はターミナルベルに依存するため、Agent 側でベル通知を有効にしておく必要がある（鳴らさない CLI では入力待ちマーカー・通知は出ない）。完了によるアイドル（`⏸`）はフックのある `claude` でのみ判定する。
- ホーム画面（サイドバー）でアイドル中のマーカー更新はキー操作のたびに反映される（通知は検知の時点で即時に出る）。アタッチ中は定期再描画でライブ更新される。
