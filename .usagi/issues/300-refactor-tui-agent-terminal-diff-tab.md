---
number: 300
title: refactor(tui): Agent・terminal・diff の tab 起動フローを統一する
status: done
priority: high
labels: [tui, pane, ux]
dependson: []
related: [141, 148, 232, 295]
created_at: 2026-07-14T22:33:19.505265+00:00
updated_at: 2026-07-14T22:41:29.084963+00:00
---

## 目的

Agent・terminal・diff の起動を共通の tab ライフサイクルへ統一する。起動要求時は shared pending wave を表示し、成功時だけ対応する live tab を選択する。

## 受け入れ条件

- Agent、terminal、diff は同じ pending tab 作成経路を使う。
- pending 表示は既存の共有 shimmer wave を使う。
- 起動完了時は対応する pending tab を live tab に置換し、その tab を選択する。
- failure・重複・stale completion は安全に収束し、別 tab の選択を奪わない。
- reducer / presentation の回帰テストを追加または更新する。
- v2 documentation を更新し、品質 gate を通して PR を作成する。
