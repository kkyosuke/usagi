---
number: 124
title: refactor(tui): 在席メニューの行ビルダ重複（chevron/pick-row/selected 強調）を共通化する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: [123]
created_at: 2026-07-04T23:15:47.624956+00:00
updated_at: 2026-07-04T23:15:47.624956+00:00
---

## 背景（なぜ問題か）

`panes.rs` の在席メニュー群に強い重複がある。

- **展開シェブロン計算**（`▾`/`▸`/`"  "`）が `focus_agent_command_row` / `focus_terminal_command_row` / `focus_close_command_row` の 3 箇所でほぼ同型に書かれ、差分は状態述語だけ。
- **サブ行レイアウト**（`menu_marker` + `style(format!("{:<N}")).accent()/.cyan()` の selected 時 `.bold()` + dim タグ + 6 スペースインデントで `clip_to_width`）が `focus_agent_pick_row` / `focus_close_pick_row` / `focus_terminal_pick_row` に三重化。
- **selected 強調分岐**（`if selected { style(x).bold() } else { style(x) }`）が `name_cell` / `menu_row` / 各 pick_row / `create_row` / `rail_create_row` 等に散在。

文字幅（9/10/14）や色（accent/cyan）が微妙に食い違っており、意図せぬドリフト源になっている。

## 対象箇所

`src/presentation/tui/home/ui/panes.rs`: `focus_agent_command_row` / `focus_terminal_command_row` / `focus_close_command_row`（chevron）、`focus_agent_pick_row` / `focus_close_pick_row` / `focus_terminal_pick_row`（pick-row）、`menu_row` / `name_cell` 等（selected 強調）。

## やること

- `expand_chevron(open, can_expand, reserve) -> &str` を抽出する。
- `pick_row(label, name_col, tag, selected, width)` を抽出して 3 つの pick-row を委譲する。
- `emphasise(text, selected)`（selected で bold）ヘルパを導入し強調分岐を集約する。色/幅は引数化して SSoT 化する。

## 受け入れ条件

- 3 つの pick-row と 3 つの chevron 計算が共通ヘルパ経由になり、行差分が「引数の違い」だけになる。
- 既存の在席メニュー描画テストが無変更で通過。カバレッジ 100% 維持。

## 補足

#123（panes.rs 分割）で `focus_menu.rs` に切り出す対象と同一領域。分割後に共通化すると綺麗にはまるため related。
