---
number: 437
title: refactor(tui): presentation/mod.rs（5,727 行）を分割し worker thread 直 spawn を port 化する（module doc「実 IO は持たず」との矛盾解消）
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:01:37.819706+00:00
updated_at: 2026-07-20T12:01:37.819706+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/presentation/mod.rs` は **5,727 行**。ports・keymap・画面遷移・イベントループ・レンダリング接続が同居。
- module doc :4 は「実 IO は持たず、出力先は呼び出し側（合成ルート）から注入する」と宣言しているが、`std::thread::spawn(move || {` の直呼びが **:1199, :1327, :1353** に存在し自己矛盾。

## 問題

- 1 ファイルに責務が集中し、変更影響の追跡とレビューが困難。
- worker thread の直接 spawn は presentation を実 IO に結合させ、テストではスレッド実行を伴う。

## 改善案（要検討）

- ports.rs / keymap.rs / screen_graph.rs / controller_shell.rs 程度の 4 分割を目安にモジュールを切り出す（境界は実装時に調整可）。
- thread spawn 3 箇所を spawner port（`Fn(Job)` 相当）として注入化し、実スレッドは合成ルートで束ねる。module doc を実態と一致させる。

## 受け入れ条件

- [ ] mod.rs が責務ごとのモジュールに分割され、単一ファイルの行数が大幅に減る。
- [ ] presentation 内に `std::thread::spawn` 直呼びがなく、doc と実装が一致する。
- [ ] 既存挙動が回帰しない（既存テスト維持）。coverage 100% を維持。
