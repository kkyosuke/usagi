---
number: 358
title: fix(tui): pending 中に Closeup action modal が毎フレーム復活する退行を直す
status: in-progress
priority: high
labels: [tui, controller, regression]
dependson: []
related: [315, 316, 258]
created_at: 2026-07-19T11:36:42.149443+00:00
updated_at: 2026-07-19T11:36:42.149443+00:00
---

## 背景

#315（#1063 で合成ルート切替）/ #316（#1071 で旧経路削除）の controller 移行で、旧経路（#1028 = 08917912 が実装した pending tab の wave / focus 契約）が controller 経路へ移された。wave 描画自体（`pending_frame = mascot_tick` → `widgets::session_tab::pending_label`）は移行済みで動作するが、Closeup action modal の可視判定に退行が残った。

## 退行（R1）

pending（agent / terminal 起動待ち、live pane 未確立）の間、Closeup action modal が毎フレーム復活し、pending tab の wave による起動中表示を右ペインごと覆ってしまう。

- 投影側 `crates/tui/src/presentation/views/workspace.rs` の `HomeProjection::from_state` が
  `closeup_action_visible = (route が Closeup) && (!has_live_pane || overlay == Some(Overlay::Closeup))`
  と判定し、pending placeholder の存在を考慮していなかった。
- `render_home` は `closeup_action_visible` のとき、runtime が submit 後 `None` にした `home.closeup_modal` の
  fallback として新品の `CloseupModal` を生成して被せる。pending 中は `has_live_pane == false` のため
  `closeup_action_visible` が真になり、起動待ちの間ずっと modal が右ペインを覆っていた。
- 旧経路（#1028）では submit 後 modal は閉じ、wave が見えていた。

## 契約（明文化）

Closeup action modal を出すのは「Closeup で tab が 1 枚も無い（pending も含めて無い）とき」、
または「`Overlay::Closeup` が明示/forced で開いているとき」だけ、という単一の契約にする。
`has_live_pane`（live pane の有無）ではなく tab（pending / live / document を 1 枚と数える）の有無で
判定する。#279 の forced action overlay（`closeup_action_forced`）と `Overlay::Closeup` 明示オープンの
経路は壊さない。

## 完了条件

- 可視判定を pane strip 適用後（`HomeProjection::with_pane`）に確定し、pending tab があるときは
  action modal を自動表示しない（wave が覆われない）。launch 失敗で pending が消えたら modal が戻る。
- controller 経路の回帰テストで固定する（pending 中は modal 非表示・wave 可視、fail 後は modal 復活）。
- 仕様ドキュメント（`document/03-tui.md` の Closeup action modal 表示条件）を実態に整合させる。
- full test + coverage 100% / Markdown link check を通す。
