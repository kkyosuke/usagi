---
number: 372
title: refactor(tui): modal 共通 body-composition kit（footer/heading/empty/scroll/indent）を widgets/modal.rs に集約する
status: done
priority: medium
labels: [tui, ui, modal, refactor]
dependson: []
related: [261, 317, 244, 243]
created_at: 2026-07-19T22:00:10.433708+00:00
updated_at: 2026-07-19T23:17:45.645500+00:00
---

## 目的

TUI の全 modal/overlay が `widgets/modal.rs` の frame primitive（`boxed` / `render_over` / `render_modal` / `fixed_body` / `modal_inner_width`）に framing を委譲している一方で、その 1 段上の「body 組み立ての約束事」——footer/help 行・2 桁 body indent・空状態 notice・scroll indicator・`render` / `render_over` の双子関数・`fixed_body(min(height-4))` の reserve——は各 view で個別に再実装されている。この重複を pure な shared component として `widgets/modal.rs`（および必要なら新 `widgets/modal/` 下）に集約し、各 modal の view には固有の内容だけを残す。

これは modal component 整理の **基盤（foundation）issue** であり、confirmation 統一（別 issue）と形別コンポーネント境界（別 issue）はこの上に載る。

設計の正本: `.agents/designs/372-modal-component-refactor.md`。

## 背景（現状の重複）

調査で確認した重複パターン:

- **footer/help 行**: ほぼ全 modal が `Style::new().dim().paint("  …: …   …: …")` を最終 body 行として push し、直前に空行 spacer を置く。leading space（`"  "` の有無）と区切りが view ごとに不揃い。`remove_modal` だけ help を先頭に置く。
- **2 桁 body indent**: `clip_to_width(&format!("  {line}"), INNER_WIDTH)` の idiom が decision / scratchpad / pr / overview / closeup / remove に散在。
- **空状態 notice**: `dim().paint("  (none)")` / `(empty)` / `no pull requests` 等が 6 か所以上。
- **scroll indicator**: `↑ N more` / `↓ N more`（wording 差あり）が pr_modal と text_overlay に独立実装。
- **`render` / `render_over` 双子**: `render_modal` か `render_over` かだけ違う near-identical boilerplate が pr / overview / closeup / text_overlay に重複。
- **小端末 reserve**: `fixed_body(body, BODY_HEIGHT.min(height.saturating_sub(4)))`（text_overlay は `-2`）が同一コメントごと copy-paste。
- **選択マーカー `›`**: `Role::Danger.style().bold().paint("›") else " "` が pr / overview / closeup / remove / decision と `widgets/select.rs` に重複（select は既に持つのに modal 側が再実装）。
- **ANSI strip test helper**: pr / overview / closeup の test module に verbatim コピー。

## スコープ

- `widgets/modal.rs` に body-composition helper を追加する（pure・IO なし）。想定 API（名称は実装時に調整可）:
  - `footer(hints: &str)` — dim・一貫した leading indent の help/footer 行。
  - `heading(text)` / `caption(text)` — 見出し・小見出しの共通スタイル。
  - `empty_notice(text)` — 空状態の dim 行。
  - `content_line(text, inner_width)` — 2 桁 indent + `clip_to_width` の idiom。
  - `scroll_indicator(above: usize, below: usize)` — `↑ N more` / `↓ N more` の共通生成（wording を統一）。
  - `selection_marker(selected: bool)` — `›` マーカー（`widgets/select.rs` の既存実装と整合、または再利用）。
  - `render`（中央）/ `render_over`（背景合成）の双子と `BODY_HEIGHT.min(height-4)` reserve を畳む compose helper（例 `render_body`/`render_body_over`）。
- 対象 9 overlay（Overview / Closeup / QuitConfirmation / Notes / Environment / Decisions / Prs / Preview）に加え、同型の `remove_modal` と `open.rs` unregister も可能な範囲で helper へ寄せる。
- test module の重複 `strip` helper を test-support に 1 本化する。
- CreateSession は sidebar inline 入力であり modal 化し直さない（対象外）。

## 完了条件

- 上記 helper を pure test 付きで `widgets/modal.rs`（または `widgets/modal/`）に追加する。
- 各 view が helper 経由で footer/indent/empty/scroll/reserve を組み、**表示は byte 単位で回帰しない**（移行前後で同一 base・同一 state に対する render frame が一致することを test で固定する）。
- 選択マーカー・footer wording は現行の見た目を保ったまま 1 経路に統一する（意図的な変更が要る箇所は設計 doc に明記し、対応する test を更新する）。
- tiny terminal で panic / out-of-bounds / 背景合成範囲の逸脱を起こさない（既存の tiny-terminal test を維持・拡張）。
- coverage 100% を維持する。
- `document/03-tui.md` の modal 節に共通 body-composition の約束事を追記する。

## 対象外

- modal の幅・配色・入力 semantics・key binding・daemon request の変更。
- confirmation modal の統一（別 issue）。
- 形別（list / editor / text-viewer / palette）コンポーネント境界の抽出（別 issue）。
- CreateSession の modal 化。
