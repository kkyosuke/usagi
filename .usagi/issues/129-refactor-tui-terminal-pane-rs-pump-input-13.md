---
number: 129
title: refactor(tui): terminal/pane.rs の pump_input（13 引数・イベント種別混在）を状態構造体化・ハンドラ分割する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-04T23:16:59.509603+00:00
updated_at: 2026-07-04T23:16:59.509603+00:00
---

## 背景（なぜ問題か）

`home/terminal/pane.rs` の `pump_input` は約 350 行・引数 13 個で、キー入力（プレフィックス/スクロール/`classify` 分岐/コピー/転送）・ペースト・マウス（クリック/ドラッグ/ホイール/ホバー）を 1 つの巨大 `while` + `match Event` に詰め込んでいる。加えて `selection`/`drag_tab`/`hover`/`pending_prefix`/`last_click`/`last_pointer`/`scrollback` といった可変状態を個別の `&mut` で受け渡しており、シグネチャが肥大している。同ファイルの `drive`（約 330 行）も長い。

## 対象箇所

`src/presentation/tui/home/terminal/pane.rs`: `pump_input`（および `drive`）。

## やること

- 可変ポインタ/選択/プレフィックス状態を 1 つの `PaneInputState`（または既存の入力状態構造）にまとめて引数を削減する。
- `Event::Key` / `Event::Paste` / `Event::Mouse` の処理を `handle_key` / `handle_paste` / `handle_mouse` に分割する。`pending_bytes` の flush 契機は現状の制御フローを保つ。

## 受け入れ条件

- `pump_input` の引数が構造体化で半減し、本体が 3 ハンドラに分割される。
- `pane.rs` の入力系テストが無変更で通過。カバレッジ 100% 維持。

## 補足

#57（todo、`event_loop`・handlers の context 構造体化）とは対象ファイルが別＝`terminal/pane.rs` の入力ポンプに限定。
