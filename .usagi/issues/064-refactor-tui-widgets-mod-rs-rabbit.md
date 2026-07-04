---
number: 64
title: refactor(tui): widgets/mod.rs から rabbit アセットを分離する
status: done
priority: low
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-06-19T22:17:23.832740+00:00
updated_at: 2026-07-04T00:05:39.047152+00:00
---

## 背景

`src/presentation/tui/widgets/mod.rs`（954 行、非テスト ~440 行）に、汎用 widget primitive（`clip_to_width` / `centered_padding` / `boxed` / `render_modal` / `chooser` / `block_caret`）と、マスコット ASCII アート 6 関数（`rabbit_lines` / `loading_rabbit` / `loading_rabbit_timed` / `done_rabbit` / `running_rabbit` / `multiplying_rabbits`）が同居している。後者は「共通 widget」というより演出アセット。

## 対応方針

- rabbit 系 6 関数（と対応テスト）を `widgets/rabbit.rs`（または `mascot.rs`）へ分離する。
- `widgets/mod.rs` の責務を「レイアウト / ボックス / カラー primitive」に純化する。

## 確認方法

- 各画面の描画が変わらないこと（既存テスト維持）。
- カバレッジ 100% 維持。
