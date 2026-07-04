---
number: 123
title: refactor(tui): panes.rs（4696 行）をペイン種別ごとのサブモジュールに分割する
status: todo
priority: high
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-04T23:15:28.342337+00:00
updated_at: 2026-07-04T23:15:28.342337+00:00
---

## 背景（なぜ問題か）

`home/ui/panes.rs` は 4696 行あり、互いに独立した多数の描画責務が単一ファイルに同居している。左サイドバー（`left_pane`/`rail_pane`/行ビルダ群/スクロール計算 `LineWindow`）、在席メニュー（`focus_*` 群）、タブストリップとヒットテスト（`tab_strip_parts`/`*_tab_at`/`*_tab_hit`）、PR ポップアップ（`pr_popup_*`）、右ペインのディスパッチ（`right_pane_contents`）、**diff 描画**（`diff_pane`〜`pad_diff`, `DIFF_*_BG` 定数群、約 270 行）、**Markdown 描画**（`markdown_row`/`styled_span`/`heading_style`/`rgb_to_ansi256`, 約 60 行）まで詰め込まれている。ファイル冒頭のドックコメントは「二ペインの body」を謳うが実態と乖離しており、変更時の影響範囲把握・レビュー・テスト分離が困難。規約「1 ファイル 300 行超で分割検討」に大きく反する土台ファイル。

## 対象箇所

`src/presentation/tui/home/ui/panes.rs` 全体。特に diff 描画ブロック（`diff_pane`〜`in_changed`, `DIFF_ADD_BG` 等）と Markdown 描画（`markdown_row`〜`rgb_to_ansi256`）は独立性が高い。

## やること

- `home/ui/` 配下に以下を切り出す: `sidebar.rs`（行ビルダ・`DetailCols`・`LineWindow`・スクロール計算）、`focus_menu.rs`（`focus_*`）、`tabs_hit.rs`（タブストリップ＋各ヒットテスト）、`pr_popup.rs`、`diff_render.rs`、`markdown_render.rs`。
- `panes.rs` は `right_pane_contents` を中心とした薄いディスパッチャに残す。`pub(super)` 境界は現状維持できる分割単位を選ぶ。

## 受け入れ条件

- 各新モジュールが 300 行前後以内。公開シグネチャ（`left_pane`/`right_pane_contents` 等）の呼び出し側無改変。
- `cargo test presentation::tui::home::ui` が全通過。既存の `ui/tests/*` が分割後も同じ描画結果を検証。カバレッジ 100% 維持。

## 補足

最大ファイルかつ最も多くの描画呼び出しの土台。diff/Markdown 描画から切り出すと波及なく効果大。在席メニュー DRY（別 issue）・AgentState 集約（別 issue）の受け皿にもなる。
