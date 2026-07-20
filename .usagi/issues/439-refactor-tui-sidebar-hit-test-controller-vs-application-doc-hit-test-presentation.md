---
number: 439
title: refactor(tui): sidebar hit-test の方針を統一する（controller のセル単位ミラー vs application doc「hit-test は presentation の責務」）
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:02:02.765343+00:00
updated_at: 2026-07-20T12:02:02.765343+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/usecase/application/controller.rs:956-1010` — `sidebar_selection_at` が描画レイアウト（chrome 行・左右分割・mascot 予約・スクロールオフセット）を**セル単位でミラー**する。doc（:950-955）自身が「`home_left_pane` render をミラーする」と二重管理を認めている。
- `crates/tui/src/usecase/application.rs:251-253` — `Key::Click` の doc は「画面ごとの hit test は presentation が担い、座標を reducer や domain へは渡さない」と**正反対の方針**を宣言している。

## 問題

方針が矛盾したまま両実装が併存し、レイアウト変更のたびに controller 側ミラーが黙ってずれる（クリックと描画の不一致）。新規画面がどちらの流儀に従うべきか判断できない。

## 改善案（要検討）

- どちらかに統一する:
  - reducer 側で hit-test するなら、レイアウトを純関数化して render 側も**同じ関数**を消費する（ミラー廃止）。
  - presentation 側で hit-test するなら、Click は解決済みのセマンティックイベントとして reducer へ渡し、`sidebar_selection_at` を presentation へ移す。
- 採らなかった側の doc を修正する。

## 受け入れ条件

- [ ] hit-test の所在が 1 方針に統一され、doc と実装が一致している。
- [ ] レイアウト定義が 1 箇所になり、描画とクリックの整合がテストで固定されている。
- [ ] coverage 100% を維持する。
