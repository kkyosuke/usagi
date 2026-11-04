---
number: 777
title: "feat(tui): workspace 横断の対応待ち一覧を追加する"
status: done
priority: high
labels: [v2, tui, workspace]
dependson: []
related: []
created_at: 2026-10-09T00:00:00+00:00
updated_at: 2026-10-09T00:00:00+00:00
---

## 目的

開いている全 workspace の人の対応待ち・システム待ち・停止を横断して確認する。

## 受入条件

- 常時背景観測し、project tab に対応待ち件数を表示する。
- 横断一覧で workspace/session と待ち理由を表示し、対象へ移動できる。
- 未取得・取得失敗をゼロ件と扱わない。
- stable identity と daemon authority、frame thread の IO 禁止を維持する。
- テスト・仕様・PR レビュー・CI を完了する。
