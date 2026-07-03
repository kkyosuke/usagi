---
number: 92
title: fix(tui): 端末リサイズ追従と再描画の堅牢化
status: todo
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:21.606469+00:00
updated_at: 2026-07-03T23:21:21.606469+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 問題
1. welcome / config / new / open は**キーを押すまで旧サイズのまま**（ブロッキング read）。
2. home も EINTR で起きるが `skip_paint` が状態変化しか見ないため、リサイズだけでは再描画されないことがある（`home/event/mod.rs`）。
3. サイズが変わっても `painter.reset()` されないため、端末側がリサイズで reflow/クリップした後に「差分なし」と判断された行が壊れたまま残り得る。

## 対応
- 各ループで `(height,width)` を前回値と比較し、変化時は `painter.reset()` ＋強制 repaint。
- home は `skip_paint` の条件に size 変化を追加。
- key-only 画面は `read_key_timeout` ベースへ寄せて低コストで追従。

## 受け入れ条件
- 各画面で端末リサイズ後、キー入力なしで正しく再レイアウトされる。
- リサイズ後に壊れた行が残らない。カバレッジ 100% 維持。
