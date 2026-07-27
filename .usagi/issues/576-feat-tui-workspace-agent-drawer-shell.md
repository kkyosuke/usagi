---
number: 576
title: feat(tui): Workspace Agent の右 drawer shell と入力導線を追加する
status: todo
priority: high
labels: [v2, tui, ux, drawer]
dependson: [575]
related: [388, 506, 510, 545]
parent: 571
created_at: 2026-07-27T23:03:34.156850+00:00
updated_at: 2026-07-27T23:03:34.156850+00:00
---

## 背景

Epic #571 は workspace root Agent を managed session の Closeup から分離し、Home 右端の overlay drawer へ移す。#575 が sidebar/navigation から root target を取り除いた後、本 issue は **Agent runtime をまだ接続しない drawer shell、表示 geometry、入力 ownership** を追加する。

描画と入力の基礎を runtime/永続化から分離し、狭幅・overlay precedence・開閉時の背景 state 保持を pure にレビュー可能にする。

## 対象責務

- Home header 右側に `Workspace Agent` button を追加する。notice badge / mode toggle / workspace breadcrumb と同じ layout authority で表示幅と click hit-test を計算する。
- `LiveTerminalAction::WorkspaceAgent`（名称は実装に合わせてよい）を追加し、`Ctrl-O g` を Switch / managed-session Closeup / live pane から同じ drawer toggle action へ解決する。
- Home の最前面 surface として `WorkspaceAgentDrawer` の open/closed state を追加する。drawer open 中は背景 sidebar、managed pane、header の別 action、通常 overlay へ入力/clickを伝播しない。
- Esc と再度の `Ctrl-O g` で drawer を閉じる。close 後は開く前の cursor、active managed session、Home mode、selected managed tab、scroll/selection stateを変更しない。
- Home header 下から右端に重なる drawer frame を描く。通常幅は約60%、上限96 columns、下限56 columnsを目安に clampし、背景と併存できない狭幅では全幅へ縮退する。
- drawer の empty state、conversation selector placeholder、`New` affordance、footer を描く。ただし本 issue ではAgent inventory、live terminal、picker launchを接続せず、それらを受け取れる presentation model / event seamまでを提供する。
- drawer 固有の terminal viewport geometry を pure function として定義し、背景 Closeup viewport と混同しない。
- 既存 modal（Overview、Closeup action、Config、decision、notes、create/error、quit等）との優先順位を明示する。既存 modal が前面にある間はdrawer entryを処理せず、drawerから既存modalを暗黙に開かない。

## 非対象

- root Agent inventory、tab intent、attach/input/resize/resume（#577）。
- New の CLI pickerとlaunch（#578）。
- shipping process E2Eと全体docs確定（#579）。
- provider transcriptをparseするchat renderer。

## 受入条件

- [ ] header button と `Ctrl-O g` が Switch / managed Closeup / live pane の全入口から同一drawer stateをtoggleする。
- [ ] drawer open/closeでmanaged-session cursor/active/mode/tab/scroll/selectionが変わらない。
- [ ] drawer open中は背景のsidebar click、pane input、global actionへ入力が漏れない。
- [ ] 既存modalが入力所有中はdrawer shortcut/header clickがeffect zeroで、modalを壊さない。
- [ ] 通常幅、境界幅、極小幅、0 geometry、resize、CJK workspace名、notice badgeありで、renderとhit-testが一致しpanic/style leakしない。
- [ ] drawer viewport geometryが背景right paneと独立して計算され、後続runtimeが利用できる。
- [ ] empty drawerは自動Agent launch/resumeを発行せず、placeholderとNew affordanceだけを表示する。

## 必須テスト

- controller/runtime: open/close/toggle、overlay precedence、background state snapshotの不変性。
- terminal input classifier: `Ctrl-O g`、unknown follow-up、leader timeout、live passthrough非侵害。
- presentation: header layout/hit-test、right overlay合成、dim、56/96 clamp、全幅fallback、resize/CJK/notice。
- screen graph: direct/Welcome/Recent/Openの各workspace entryで同じdrawer shellが使われる。
