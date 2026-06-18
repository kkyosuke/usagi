---
number: 38
title: perf: TUI ターミナルペイン描画をイベント駆動化し無駄な再構築を削減する
status: todo
priority: high
labels: [perf, tui]
dependson: []
related: []
created_at: 2026-06-17T22:50:37.695828+00:00
updated_at: 2026-06-17T22:50:37.695828+00:00
---

## 背景

`src/presentation/tui/home/terminal_pane.rs` の描画ループは、出力も入力もないアイドル時でも常時 CPU を消費している。複数のレビュアーが独立に指摘した高信頼の問題。

### 1. `wait` ループのアイドル時フル再描画（`terminal_pane.rs:154-167`）
`event::poll(4ms)` を回し続け、`IDLE_REDRAW=100ms` ごとに必ず全サイクル（`pty.resize`=ioctl → parser ロック → `from_screen` で全グリッド文字列化 → `render_frame` 全行生成 → diff）を実行する。完全アイドルの端末で毎秒 10 回、全グリッドを走査して `TerminalView` を再生成し、リーダースレッドの parser ロックと競合する。

### 2. 毎フレーム無条件 `pty.resize()` + 全グリッド `from_screen`（`terminal_pane.rs:101-124`、`terminal_view.rs:35-67`）
サイズ未変化でも毎フレーム `resize`(TIOCSWINSZ) と `set_scrollback` で parser ロックを 2 回取得。`from_screen` は容量ヒントなしの `String::new()` で行を作り、`CellStyle::sgr()` がスタイル変化のたび `format!` で一時 String を 2 回確保（色の多い画面で多発）。

### 3. 毎フレーム 4 つの `HashSet<PathBuf>` を 4 回ロックして clone（`terminal_pane.rs:120-123`、`terminal_pool.rs:107-134`）
`running()/waiting()/live()/done()` を別々に呼び、1 フレームで mutex を 4 回ロック・各 PathBuf を個別 clone した HashSet を 4 つ作って捨てる。watcher は 200ms 周期でしか更新しない。

### 4. `clip_to_width` が O(n²)（`home/ui/mod.rs:121-140`）
1 文字進むごとに `out.clone()` し、`measure_text_width` を先頭から再計測。全ワークツリー行・ログ行・ターミナル全行に毎フレーム適用。

### 5. メインイベントループの無期限ブロック（`home/event/mod.rs:116-150`）
`read_key()` で無期限ブロックするため、背景セッションが waiting/done に変化してもキー入力まで画面バッジが更新されない。

## 改善方針

- generation 変化をリーダースレッドから Condvar/mpsc で起こすイベント駆動化。最低限 `poll` タイムアウトを伸ばし、`IDLE_REDRAW` の周期再描画はリサイズ検出時のみに限定。
- 直前の `(rows,cols)` を保持し変化時のみ `resize`/`set_scrollback`。
- monitor は 1 回のロックでまとめて取る `snapshot()` にし、generation 不変なら clone をスキップ。
- `from_screen` は `String::with_capacity(cols)`、`sgr()` は `write!` で line に直接書き込み一時確保を排除。
- `clip_to_width` は累積幅を 1 パスで管理（`push`→超過なら `pop`）。
- メインループをタイムアウト付き poll に変更し、背景バッジを定期更新＋アイドル時ゼロコスト化。

## 確認方法

- アイドル時の CPU 使用率がほぼ 0 になること。
- キー入力反映・リサイズ追従に体感劣化がないこと。
- 既存 E2E テストが通ること（カバレッジ 100% 維持）。
