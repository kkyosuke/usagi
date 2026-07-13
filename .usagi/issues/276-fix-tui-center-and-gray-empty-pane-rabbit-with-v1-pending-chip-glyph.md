---
number: 276
title: fix(tui): center and gray empty pane rabbit with v1 pending chip glyph
status: done
priority: high
labels: [tui, parity, runtime]
dependson: []
related: [263, 265, 271, 274]
created_at: 2026-07-13T02:29:39.315706+00:00
updated_at: 2026-07-13T02:33:52.486581+00:00
---

## 目的

右ペインの空状態を v1 parity に揃え、pending tab は v1 の選択 session icon を一文字だけ使う frame 表示にする。

## 実装

- 空 pane は static rabbit、案内文、safe feedback をそれぞれ右ペイン幅の中央へ置く。
- rabbit と caption は明示的な white + dim（灰色）で描画し、clip 後に style を適用して必ず SGR reset を閉じる。
- pending chip は v1 選択 session gutter の Nerd Font glyph `󰤇`（U+F0907）だけを赤/黄で frame ごとに切り替える。label 全体を着色しない。
- pending → live/failure の identity reducer は既存 `PaneState` を維持し、frame 表示は state transition を変更しない。

## 検証

- empty state の中央配置、灰色 ANSI reset、狭幅 clipping を widget test で検証する。
- pending glyph の 1-column 幅、v1 glyph、reset を検証する。
- Home tick により pending chip の frame が変化し、pending reducer state が不変であることを view test で検証する。

## 後続

実端末 runtime への Home/Panes lifecycle 合成は #277 が担当する。`src/runtime/tui.rs` はまだ legacy Workspace loop を起動しているため、この issue では pure widget/view parity に限定する。
