---
number: 147
title: refactor(tui): Home event handler を reducer と effect runner に分離する
status: done
priority: medium
labels: [refactor, tui, review]
dependson: [143, 141]
related: [45]
parent: 137
created_at: 2026-07-06T00:22:01.926472+00:00
updated_at: 2026-07-06T00:22:01.926472+00:00
---

## 目的

`event/mod.rs` / `event/handlers.rs` の巨大 match を、small reducer と side-effect runner に分離し、Focus / Switch / Attached から戻った後の状態遷移をテストしやすくする。

## 背景

現在の handler は key dispatch、`HomeState` 更新、PTY 起動、config 画面、git diff、background task dispatch、browser open、paint reset などを同じ関数内で実行する。`Wiring` に side-effect closure はまとまっているが、key -> 状態遷移 -> effect の境界が曖昧で、テストは event loop に近い fake wiring を必要とする。Focus action と TabModel を切り出した後は、remaining handler も reducer 化しやすくなる。

## 変更方針

- `HomeInput` / `HomeEffect` / `HomeTransition` の小 vocabulary を追加する。
- reducer は mode / overlay ごとに小さく分ける。
  - `reduce_switch_key`
  - `reduce_focus_key`
  - `reduce_palette_key`
  - `reduce_overlay_key`
- effect runner は既存 `Wiring` を使い、IO をここだけに閉じる。
  - launch pane / reattach pane
  - dispatch create/remove/update
  - open config
  - open external terminal / URL
  - read diff / preview
- 一度に全 event loop を置換せず、Focus 周辺から段階移行する。

## 対象ファイル

- `src/presentation/tui/home/event/mod.rs`
- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/state/mode.rs`
- `src/presentation/tui/home/state/modal.rs`
- `src/presentation/tui/home/event/tests/*.rs`
- `src/presentation/tui/home/state/tests/*.rs`

## 受け入れ条件

- Focus / Switch の主要 key が reducer 単体テストで確認できる。
- `Wiring` closure を直接呼ぶ箇所が effect runner に集約される。
- 既存 event tests が通り、UI/キー挙動は変わらない。
- 後続で overlay や palette も同じ pattern に寄せられる余地が残る。

## テスト方針

- reducer 単体テストを追加する。
- `cargo test focus_menu`
- `cargo test focus_prompt`
- `cargo test background_tab`
- `cargo test switch_mode`

## 非目標

- event loop 全体の rewrite は行わない。
- `Wiring` の全 closure をこの issue で再設計しない。
- 実 PTY / terminal の integration は増やさない。
