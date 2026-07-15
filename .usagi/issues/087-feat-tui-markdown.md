---
number: 87
title: feat(tui): Markdown プレビューを折り返し表示する
status: done
priority: high
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:20:16.343759+00:00
updated_at: 2026-07-03T23:20:16.343759+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 背景 / 問題
Markdown プレビューは**折り返しなし**で行をペイン幅にクリップする（`src/presentation/tui/home/ui/panes.rs` の `markdown_row` → `clip_to_width`、`preview_pane`）。

- ソースがハードラップされていない文書（1 段落 = 1 行の README は多い）では段落の大半が `…` で消え、プレビューの主目的（README を読む）が果たせない。
- CJK 対応の `wrap_to_width` は既にあるのに、使っているのはマスコットの吹き出し（`widgets/rabbit.rs`）だけ。

## 対応
- `MarkdownLine` → 表示行への展開時に `wrap_to_width`（プレフィックス分のぶら下げインデント付き）で折り返す。
- スクロール総数も**折り返し後の行数**で数える。
- コードブロックのみ現行どおりクリップで良い（横スクロール前提）。

## 受け入れ条件
- 長い段落がプレビュー幅で折り返して全文読める。
- CJK が途中で割れない。スクロール位置表示が折り返し後の行数と一致。
- カバレッジ 100% 維持。
