---
number: 144
title: refactor(tui): TerminalPool を TabModel 適用と PTY IO ownership に分割する
status: done
priority: medium
labels: [refactor, tui, terminal, review]
dependson: [141]
related: [128, 129]
parent: 137
created_at: 2026-07-06T00:21:43.399292+00:00
updated_at: 2026-07-06T00:21:43.399292+00:00
---

## 目的

`TerminalPool` の tab 操作ロジックを #141 の `TabModel` に委譲し、pool は PTY ownership / spawn / watcher 登録に集中させる。

## 背景

`terminal/pool.rs` は pane list、active index、label cache、rename/move/close、spawn、watcher、PR scan、live prompt delivery を抱えている。既存 #128 は watcher / PR scan 分離、#129 は `pane.rs::pump_input` 分割を扱う。この issue はその前後どちらでも進められる tab 状態の責務分離に絞る。

## 変更方針

- `SessionPanes` 内の active index / labels / label override 操作を `TabModel` に寄せる。
- `TerminalPool` は `Pane` 実体（`PtySession`）と `TabModel` の id 対応を持つ。
- `nav` / `swap_active` / `move_tab` / `move_tab_by` / `rename_tab` / `close_tab` / `close_active` / `tabs` は `TabModel` の結果を適用する薄い wrapper にする。
- `snapshot_open_panes_for` は `TabModel` から active / label override を読む。
- watcher 登録時の pane order / active pane / agent pane 検出は現状維持する。

## 対象ファイル

- `src/presentation/tui/home/terminal/pool.rs`
- `src/presentation/tui/home/terminal/tabs.rs`
- `src/presentation/tui/home/terminal/mod.rs`
- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/event/tests/background_tab.rs`

## 受け入れ条件

- `TerminalPool` の公開 API は原則維持する。
- tab 操作の index arithmetic が pool から `TabModel` に移る。
- pane close / active 変更 / label override / open-pane snapshot の既存挙動が一致する。
- `pool.rs` の tab 操作に関する分岐が減り、watcher 分離 issue（#128）と競合しにくい形になる。

## テスト方針

- `cargo test terminal::tabs`
- `cargo test background_tab`
- `cargo test event::tests::focus_menu` のうち tab 関連テスト
- `TerminalPool` の実 PTY を必要としない範囲は `TabModel` 単体テストで担保する。

## 非目標

- watcher / PR scan / resource sampling の分離は #128 に任せる。
- `pane.rs::pump_input` の key/mouse 分割は #129 に任せる。
- PTY spawn や process kill の挙動は変更しない。
