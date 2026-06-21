---
number: 65
title: perf(tui): エージェント高頻度出力時の再描画コアレス・リンク全走査キャッシュ・Switch プレビュー差分化
status: done
priority: high
labels: [perf, tui, review]
dependson: []
related: [62]
created_at: 2026-06-20T12:04:09.247121+00:00
updated_at: 2026-06-20T22:07:23.704348+00:00
---

## 背景

コードレビューで判明した、エージェント実行中（高頻度出力時）の CPU 消費・応答性に直結する描画ホットパスの問題。既存 #62 は home 描画ループの「毎フレーム再計算」を扱うが、本 issue は埋め込みターミナルの出力経路に固有の問題で未カバー。

### 1. 高頻度出力時の全画面再描画（高）
`src/infrastructure/pty.rs:164` の reader スレッドは PTY 出力を 8192 バイト**チャンクごと**に `generation` をインクリメントする。`src/presentation/tui/home/terminal_pane.rs:218,225-229` の `drive` は各反復で generation 変化を見て `dirty` を立て、毎回 `pty.parser().screen()` をロックして `TerminalView::from_screen_with_selection` でグリッド全セルを文字列化し直す。

→ エージェントが出力し続ける間、約 4ms ごとにフル再描画が走り CPU を恒常消費、reader スレッドとのパーサーロック競合も増える。`IDLE_REEVAL=200ms` は上限であって下限（最小再描画間隔）がない。

### 2. 毎フレームのリンク全走査（高）
`src/presentation/tui/home/terminal_view.rs:62,90,98` は描画のたびに `terminal_link::link_cells(screen)`（`src/presentation/tui/home/terminal_link.rs:163`）を呼び、全論理行を `Vec<char>` に平坦化 → URL 走査 → `HashSet<Cell>` を構築する。さらに各セルで `links.contains` / `hovered.contains` と HashSet を 2 回 lookup する。

→ スクロールバック付きの大きな端末で、出力のたびに O(全セル) のアロケーションと HashSet 構築が発生。1 と相まって高頻度再描画時のホットパスになる。ホバー変化だけのフレームでも全再検出している。

### 3. Switch プレビューが没入モードの差分化を受けていない（中）
`src/presentation/tui/home/event/mod.rs:200-208` の `Mode::Switch` は毎反復で `pool.snapshot()`（`src/presentation/tui/home/terminal_pool.rs:458-473`）を呼び、ジオメトリ未変化でも**無条件に** `session.resize`（TIOCSWINSZ ioctl ＋パーサーロック）し、`TerminalView::from_screen` で全グリッドを生成し直す。没入モード `drive` は `last_geo` 比較で resize を差分化済みなのに、プレビュー経路だけ同じ最適化がない。

## 改善方針

- `drive` の再描画を最小フレーム間隔（例 16〜30ms）でコアレスし、generation 差分があっても直近描画から一定時間内なら間引く。
- `link_cells` の結果を generation でキャッシュし、画面内容が変わらない限り再検出しない。ホバー変化のみのフレームは再走査不要にする。リンクセル表現を `HashSet<Cell>` から行範囲リスト/ビットマップへ見直す余地もある。
- Switch プレビュー側でも直近 geo / generation を保持し、変化時のみ `resize` / `from_screen` を実行する。

## 確認方法

- エージェント大量出力中の CPU 使用が低下すること。描画結果は従来どおり（既存 ui / e2e テスト維持）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。

## 対応結果

PR #234 で対応（観測される描画結果は不変、変化がない限り重い処理を行わない方針）。

1. **再描画コアレス**（`terminal_pane.rs` `drive` / `wait`）: dirty を対話起因（入力エコー・resize・スクロール・選択・hover・badge）と出力起因（generation 変化）に分離。出力起因のみのフレームを最小フレーム間隔 `MIN_FRAME=16ms`（≒60fps）でコアレスし、対話起因は即時描画で応答性を維持。`wait` に `redraw_deadline` を渡し、保留出力をデッドラインまで持ち越して busy-loop を回避（入力は常に即時）。
2. **リンク全走査のキャッシュ**（`terminal_view.rs` + `drive`）: `TerminalView::from_screen_with_links` を新設し precomputed links を受け取れるようにした（既存 `from_screen_with_selection` は `link_cells` を呼んで委譲、既存経路・テスト・snapshot は無改変）。`drive` 側で links を generation でキャッシュし、画面内容が変わらない限り再検出しない。ホバーのみのフレームは再走査しない。表現は `HashSet<Cell>` のまま（ビットマップ化は見送り、主因はキャッシュ）。
3. **Switch プレビュー差分化**（`terminal_pool.rs` `snapshot`）: `preview_cache`（dir / geo / generation / view）を追加。3 つすべて一致ならキャッシュした view を返却（パーサーロックも取らない）。dir/geo 変化時のみ `resize`、generation 変化時のみ `from_screen`。没入 `drive` の `last_geo` 差分化と同等に揃えた。

### 検証
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（1376 passed）。
- カバレッジ lines / functions ともに 100%（`terminal_pane.rs` / `terminal_pool.rs` は端末 I/O のためカバレッジ除外、`terminal_view.rs` の新メソッドに単体テスト追加）。
- ドキュメント更新は不要と判断（観測挙動を変えない内部最適化のため）。
