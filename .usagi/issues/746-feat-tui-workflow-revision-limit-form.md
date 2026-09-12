---
number: 746
title: feat(tui): 開始前フォームで修正回数の上限を選べるようにする
status: todo
priority: medium
labels: [v2, tui, workflow]
dependson: []
related: [745]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`revision_limit` は `bind` 時に 3 で固定される。domain は 1..=10 を許容し `is_valid` でも検証しているのに、
UI からも IPC からも指定できない。長い作業では 3 往復では足りず、短い作業では多すぎる。

## 方針

- 開始前フォーム（Planner / Implementer / Reviewer の並び）に修正回数の上限を追加し、左右キーで 1..=10 を選ぶ。
- 既定値は現状と同じ 3 とする。
- 担当の組合せと同じく workspace 単位で保存し、次回の初期値に使う。
- 開始後は変更しない（上限の引き上げは #745 の再開始、または別 issue の扱いとする）。

## 受入条件

- [ ] 開始前に 1..=10 の範囲で上限を選べる。
- [ ] 既定値は 3 で、選んだ値が workspace に保存される。
- [ ] 範囲外の値は受理されない。
- [ ] 開始後のフォームは上限を変更しない。
- [ ] `document/03-tui.md` の表を更新する。
