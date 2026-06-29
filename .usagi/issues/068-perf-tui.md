---
number: 68
title: perf(tui): アイドル時のポーリング起床・常駐スクロールバックの低優先最適化
status: done
priority: low
labels: [perf, tui, review]
dependson: []
related: [41, 62]
created_at: 2026-06-20T12:04:38.689295+00:00
updated_at: 2026-06-29T00:54:43.994451+00:00
---

## 背景

コードレビューで判明した、アイドル時の無駄な起床・常駐メモリに関する低優先の改善。実害は限定的だが規模が増えると効いてくる。

## 対応内容

- アタッチ中の端末入力待ちを、直近 1 秒に出力/入力がある間は 4ms poll、完全アイドル時は 32ms poll にバックオフするようにした。
- 端末プールの watcher は、ライブセッションが無い間 `Shared` mutex を取得せず、atomic flag の確認だけで次 tick へ進むようにした。
- スクロールバックは `terminal_scrollback_lines` 設定で制御され、既定値は 2,000 行に抑えられている。`vt100` は既存 parser の cap を実行中に縮小する公開 API を持たないため、非アクティブペインだけの追加縮小は挙動を変えずには行わない。

## 確認方法

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `. scripts/coverage.sh && cargo llvm-cov --workspace --ignore-filename-regex "$COVERAGE_IGNORE" --fail-under-lines "$COVERAGE_MIN" --fail-under-functions "$COVERAGE_MIN"`

関連: #41 / #62（軽微 perf・毎フレーム再計算の既存まとめ）。
