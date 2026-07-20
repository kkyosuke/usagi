---
number: 426
title: refactor(tui): 右ペインのジオメトリ不変条件（タブ帯 3 行＋フッタ 2 行）を 3 表現から定数 1 系統へ一本化する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:58:20.798020+00:00
updated_at: 2026-07-20T11:58:20.798020+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/tui/src/presentation/views/workspace.rs` に同じジオメトリ前提が 3 つの表現で分散している:

1. `:63` — リテラル `5`（`height.saturating_sub(CHROME_ROWS + 5)`）。
2. `:910-912` — 定数 `RIGHT_PANE_CONTENT_TOP = 3` / `RIGHT_PANE_FOOTER_GAP = 2`（`terminal_point_at` のみが使用）。
3. `:1560` — `height.saturating_sub(rows.len() + 2)`。

## 問題

タブ帯やフッタの行数を 1 箇所変えると、他 2 表現（特にマウスヒットテストの `terminal_point_at`）が黙ってずれ、クリック位置と描画の不一致が起きる。

## 改善案（要検討）

- 定数（`RIGHT_PANE_CONTENT_TOP` / `RIGHT_PANE_FOOTER_GAP`）へ一本化し、リテラル `5` と `rows.len() + 2` を定数からの導出に置き換える。
- ヒットテストと描画が同じ定数を消費することをテストで固定する。

## 受け入れ条件

- [ ] ジオメトリ前提の表現が 1 系統になり、リテラルの重複が消える。
- [ ] 定数変更がヒットテスト・描画の双方に一貫して反映されることがテストで固定されている。
- [ ] coverage 100% を維持する。
