---
number: 356
title: test(tui): mascot metrics 投影の presentation ロジックを coverage 対象に戻す
status: done
priority: low
labels: [tui, test]
dependson: []
related: [258]
created_at: 2026-07-19T11:04:42.100664+00:00
updated_at: 2026-07-19T11:26:22.831287+00:00
---

## 目的

issue #258 PR1 で controller 経路へ移した mascot sidecar の daemon metrics 投影（`crates/tui/src/presentation/views/workspace.rs`）のうち、純粋な presentation ロジック 3 関数が `#[coverage(off)]` で計測から外れている。規約（[06-conventions.md 品質チェック](../../document/06-conventions.md)）は `#[coverage(off)]` を「実 IO そのもの」または generic 単相化の重複計上に限ると定めており、業務/表示ロジックの計測回避には使わない。該当関数は IO を持たない純関数なので計測対象へ戻し、未検証分岐にテストを足す。

## 対象関数

- `load_style(value, busy, hot) -> Style` — 閾値による色分岐（white/dim・yellow・red）。既存テストは white/dim 分岐しか踏まない。
- `format_memory(bytes) -> String` — MB / GB 整形（GB は小数第一位）。既存テストは MB 分岐しか踏まない。
- `mascot_metrics(Option<&DaemonMetrics>, frame) -> Vec<String>` — None（shimmer）/ Some（計測行）。両分岐とも既存テストで到達済みだが coverage 未強制。

## 変更内容

- 上記 3 関数から `#[coverage(off)]` を外す。
- `load_style` の 3 分岐、`format_memory` の MB / GB（整数・小数）分岐を直接検証する unit test を追加する。`Style` / `Color` は `PartialEq, Eq` を derive しているため等値比較で検証できる。

## 対象外

- `with_metrics` / `render_home` の描画契約や parity（#1017 で確立済み）は変更しない。
- runtime ループ・旧経路には触れない（旧経路は #316 で削除済み）。

## 完了条件

- 3 関数が coverage 計測対象になり、workspace coverage 100% を維持する。
- `load_style` / `format_memory` の全分岐が unit test で検証される。
