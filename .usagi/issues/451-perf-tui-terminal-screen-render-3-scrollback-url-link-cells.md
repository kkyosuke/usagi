---
number: 451
title: perf(tui): terminal_screen の render 3 変種を統合し、毎フレームの scrollback 全再構築による URL 走査（link_cells）をキャッシュ化する
status: todo
priority: medium
labels: [perf, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:04:57.357788+00:00
updated_at: 2026-07-20T12:04:57.357788+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/tui/src/usecase/application/terminal_screen.rs`:

- render 変種が 3 つ: `rows_with_scrollback`（:195）・`rows_with_scrollback_and_cursor`（:245）・`rows_with_scrollback_and_cursor_selection`（:273）— いずれも `link_cells()` を呼ぶ。
- `link_cells()`（:226-238）は**毎呼び出し**で `scrollback.iter().chain(&grid).map(|row| ...collect())` により全行を `Vec<String>` に再構築し、`scan_links` で URL 全走査する。scrollback 上限は **10,000 行**（:592）。
- セルごとの style が String 保持でアロケーション過多。

## 問題

高频出力時、60Hz の描画ごとに最大 1 万行の文字列再構築＋正規表現的走査が走り、CPU を浪費する（agent の大量出力で顕在化）。

## 改善案（要検討）

- 3 変種を `RenderOptions`（cursor/selection の有無）を取る 1 実装に統合する。
- `link_cells` を「scrollback/grid の世代（変更カウンタ）」でキャッシュし、変化のないフレームでは再計算しない。scrollback 部分は append-only なので増分走査にできる。
- style を intern（`Rc<str>` / enum 化）してセルごとの String 確保を減らす。

## 受け入れ条件

- [ ] render が 1 実装＋options になる。
- [ ] 無変化フレームで link 走査が走らないことがテストで固定されている。
- [ ] 表示（リンク検出・cursor・selection）が回帰しない。coverage 100% を維持。
