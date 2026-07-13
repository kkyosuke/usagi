---
number: 261
title: fix(tui): モーダル表示中の高さを固定する
status: done
priority: high
labels: [tui, ui, modal]
dependson: []
related: [259]
created_at: 2026-07-13T00:09:23.759053+00:00
updated_at: 2026-07-13T00:14:33.467387+00:00
---

## 目的

Overview を含む TUI の全モーダルで、候補数、help/result/error、editor 値、loading 状態の変化によって外枠の高さが揺れないようにする。

## スコープ

- 共通 modal widget と全 modal view の body 構成を調査し、各 modal の開時に確定する高さ、または view ごとの固定 body row 数を一貫して適用する。
- Overview、Closeup、PR、Notes / Environment、Text overlay、Create session、Quit confirmation を対象にする。
- 内容が少ない状態は空行で reserve し、内容が多い状態は既存の width / height clipping を守る。
- tiny terminal で panic / out-of-bounds / frame escape を起こさない。

## 受け入れ条件

- 各 modal は open 後に candidate/result/error/loading/empty の切替で render height を変えない。
- 小さい terminal では既存 clipping に収まり、背景合成の範囲を越えない。
- view ごとの representative state transition と common widget の pure regression test を追加する。
- 実装済み仕様 document を更新する。

## 対象外

modal の幅、配色、入力 semantics、daemon request を変更しない。
