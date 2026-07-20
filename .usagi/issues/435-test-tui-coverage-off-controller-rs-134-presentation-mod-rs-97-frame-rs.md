---
number: 435
title: test(tui): coverage(off) の棚卸し（controller.rs 134 箇所・presentation/mod.rs 97 箇所・frame.rs の即剥がし可能分）
status: todo
priority: medium
labels: [test, tui, review]
dependson: [410, 412, 433]
related: []
created_at: 2026-07-20T12:00:54.239952+00:00
updated_at: 2026-07-20T12:00:54.239952+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。repo 全体の coverage(off) 850 箇所のうち、tui が最大の集積地。

## 根拠（検証済み）

- `crates/tui/src/usecase/application/controller.rs` — **134 箇所**。reducer 中枢の `update`（:2086、attr :2084）・`update_key`（:2395、attr :2394）・`update_overlay`（:2432、attr :2431）まで除外。実害例: 到達不能 match アームが gate をすり抜けて残存（#430 として独立起票済み）。
- `crates/tui/src/presentation/mod.rs` — **97 箇所**。純関数 `step_new`（:950、attr :948）・`step_open`（:1065、attr :1063）・`render_open`（:1458、attr :1457）等を含む。
- `crates/tui/src/presentation/frame.rs:113, 197` — `set_line`（:114）・`span_text`（:198）は純関数で、in-file の 20+ テスト（:399 以降）が既に実行している。**即剥がせる**。
- `views/new.rs` の 1,245 行一括除外は #433（read_dir 注入）で解消する。

## 問題

TUI の reducer・画面遷移という最も回帰しやすい層が計測されておらず、coverage 100% gate が形骸化している。

## 改善案（要検討）

- frame.rs の 2 箇所は即剥がす（テスト済み）。
- controller / mod.rs は、未使用 reducer 削除（#410）・handler 層整理（#412）・new.rs 注入（#433）の**後**に棚卸しすると差分が小さい（本 issue はこれらに依存）。
- 実 IO（thread spawn・端末）はまず port 化 issue で注入化し、残る薄い実 IO のみ off にする。

## 受け入れ条件

- [ ] frame.rs の set_line/span_text が計測対象に戻っている。
- [ ] controller.rs / presentation/mod.rs の coverage(off) が item 単位・理由付きに削減されている。
- [ ] coverage 100% を維持する。
