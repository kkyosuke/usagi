---
number: 126
title: refactor(tui): config/ui.rs の行エディタ系モーダル 3 種の重複を共通ビルダに集約する
status: done
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-04T23:16:14.749152+00:00
updated_at: 2026-07-04T23:16:14.749152+00:00
---

## 背景（なぜ問題か）

`config/ui.rs` の `setup_modal_frame` / `env_modal_frame` / `session_labels_modal_frame` はほぼ完全なコピーである。`cursor()` 取得 → `EDITOR_MODAL_VISIBLE_LINES` の固定窓スクロール `offset` 計算 → 行番号 `{:>2} ` 付与 → カーソル行 `block_caret` / 空行 `·` プレースホルダ / 通常行、末尾に空行＋`Ctrl-S 保存 …` フッタ、`render_modal(..., MODAL_INNER_WIDTH.max(56), &body)` まで同一で、違うのはヘッダ 2 行・タイトル文字列だけ。スクロールやプレースホルダ仕様を直すたびに 3 箇所を揃える必要があり、ドリフトしやすい。

## 対象箇所

`src/presentation/tui/config/ui.rs`: `setup_modal_frame` / `env_modal_frame` / `session_labels_modal_frame`（`model_modal_frame` も行ループ部分が近い）。

## やること

- `line_editor_modal_frame(raw_h, raw_w, title, header: &[&str], cursor, lines, inner_width)` を抽出し、3 関数はヘッダ・タイトルを渡すだけの薄いラッパにする。
- フッタ文字列 `Ctrl-S 保存  Enter 改行  Esc 取消` も定数化する。

## 受け入れ条件

- 3 モーダルが共通ビルダ経由になる。
- `config::ui` の各モーダルの「一定高さ維持」「カーソル追従スクロール」「空行プレースホルダ」テストが無変更で通過。カバレッジ 100% 維持。

## 補足

コピーがほぼ完全で機械的に統合でき、テストも既に揃っているクリーンな早期成果。
