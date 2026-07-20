---
number: 374
title: refactor(tui): modal を形別コンポーネント（list / text-viewer / editor / palette）へ整理する
status: done
priority: medium
labels: [tui, ui, modal, refactor]
dependson: [372]
related: [261, 317, 244, 243]
created_at: 2026-07-19T22:01:02.622286+00:00
updated_at: 2026-07-20T01:06:05.103338+00:00
---

## 目的

#372 の共通 body-composition kit を土台に、modal を「形（shape）」ごとの薄いコンポーネントへ整理する。各 modal の view には固有の state・キー・内容だけを残し、行の並べ方・scroll viewport・選択・footer といった形の共通部分を shared component に寄せる。目的は境界を明確化して重複を減らすことであり、表示・入力 semantics は回帰させない。

設計の正本: `.agents/designs/372-modal-component-refactor.md`。

## 形（shape）分類と対象

| shape | 対象 modal | 共通化する部分 |
|---|---|---|
| list | Closeup / Prs / Decisions(一覧) / remove | 選択行・カーソルマーカー・scroll viewport（`↑/↓ N more`）・footer |
| text-viewer | Preview（`text_overlay`。Prs/PR error の Unavailable 表示も含む） | 読み取り専用の縦 scroll・scroll indicator・footer |
| editor | Notes / Environment / Decisions(editor) | draft 行・section 切替・error 行・footer |
| palette | Overview / Closeup(prompt) | `TextInput` 入力行・前方一致候補・usage/help・result strip・footer |

## スコープ

- 形ごとに composition helper（例 `modal::list` / `modal::text_viewer` / `modal::editor` / `modal::palette`、名称は実装時に調整）を定義し、pr_modal / text_overlay に散在する scroll viewport 計算（`visible_bounds` / `↑ N more` / `↓ N more`）を 1 本化する。
- list 系（closeup / pr / decision / remove）のカーソルマーカー・行 clip・footer を list component に寄せる（`widgets/select.rs` の既存マーカーとの整合を含む）。
- editor 系（notes / environment / decision editor）の draft / error / footer の並びを editor component に寄せる。
- palette（overview / closeup prompt）は list + `TextInput` + help/result を組む薄い層として整理する。
- 各 modal の state・key 解釈・具体内容は view/controller に残す（component は「形」だけを持つ）。

## 完了条件

- 対象 modal が形別 component 経由で body を組み、scroll/選択/footer の重複実装が解消される。
- 表示・入力 semantics・key binding は移行前後で回帰しない（representative state transition の frame 一致 test と既存 controller test を維持）。
- tiny terminal で panic / out-of-bounds / 背景合成範囲の逸脱を起こさない。
- coverage 100% を維持する。
- `document/03-tui.md` の modal 節に形別コンポーネント境界を追記する。

## 対象外

- modal の幅・配色・入力 semantics・daemon request の変更。
- confirmation 統一（#373）と body-composition kit の追加（#372）。
- CreateSession の modal 化（sidebar inline のまま）。

## 補足（段階分割）

想定より diff が大きくなる場合は shape 単位（list → text-viewer → editor → palette）で PR を分けてよい。各段は #372 の kit に依存し、shape 間は独立に進められる。
