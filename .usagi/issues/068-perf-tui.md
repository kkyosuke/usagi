---
number: 68
title: perf(tui): アイドル時のポーリング起床・常駐スクロールバックの低優先最適化
status: todo
priority: low
labels: [perf, tui, review]
dependson: []
related: [41, 62]
created_at: 2026-06-20T12:04:38.689295+00:00
updated_at: 2026-06-20T12:04:38.689295+00:00
---

## 背景

コードレビューで判明した、アイドル時の無駄な起床・常駐メモリに関する低優先の改善。実害は限定的だが規模が増えると効いてくる。

### 1. `wait` がアタッチ・アイドル時も高頻度起床（`src/presentation/tui/home/terminal_pane.rs:276-290`、`POLL_SLICE = 4ms`）
出力も入力も無い完全アイドル時、`event::poll(4ms)` のタイムアウトごとにループが回り、`IDLE_REEVAL=200ms` に達するまで約 50 回/秒起床する。真の busy-loop ではない（poll でブロック）が、アイドルの埋め込み端末でも 250Hz で poll syscall を回す。
→ 直近の出力/入力有無で `POLL_SLICE` を可変にする（アクティブ時 4ms、無活動継続時は数十 ms）か、PTY 出力を condvar 等で通知して poll スライス依存を減らす。

### 2. ウォッチャーがセッション皆無でも常時 200ms ポーリング（`src/presentation/tui/home/terminal_pool.rs:516`）
ウォッチャースレッドはセッションが 1 つも無くても `loop { sleep(200ms); lock; prune; observe }` を回し、毎ティック `Shared` mutex を取得する。ホーム画面を開いている間ずっと 5Hz でロック取得＋空ベクタ走査が走り、`snapshot()` を呼ぶ render とロックを取り合う。
→ `sessions` が空のときはポーリング間隔を延ばす/条件変数で待機する、もしくはセッション登録時のみ起動する。

### 3. スクロールバック 10,000 行 × ペイン数の常駐メモリ（`src/infrastructure/pty.rs:65`、`SCROLLBACK_LINES = 10_000`）
各 PTY パーサーが 10k 行のグリッドを保持する。セッション × ペイン（agent + 複数 terminal）ごとに独立保持するため、多数のバックグラウンドペインを抱えると数十〜百 MB に達し得る。
→ スクロールバック行数を設定可能にする、または非アクティブ（バックグラウンド）ペインの保持行数を絞る。

## 確認方法

- アイドル時の CPU 起床回数・常駐メモリが低下すること。挙動は従来どおり。
- 各項目は独立に着手可。`cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。

関連: #41 / #62（軽微 perf・毎フレーム再計算の既存まとめ）。
