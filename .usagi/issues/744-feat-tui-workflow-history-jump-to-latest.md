---
number: 744
title: feat(tui): Workflow 履歴を最新位置へ戻す操作を追加する
status: todo
priority: low
labels: [v2, tui, workflow]
dependson: []
related: [742]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

Workflow 履歴のスクロールは PageUp / PageDown だけで、最新（最下部）へ戻るには PageDown を連打する
しかない。Home / End は入力欄のカーソル移動に割り当て済みである。

## 方針

- 履歴を最新位置へ戻す操作を追加する（入力欄のカーソル操作と衝突しない割当）。
- 新しいエントリが届いたとき、履歴が最新位置にある場合だけ追従する（過去を読んでいる最中に飛ばさない）。

## 受入条件

- [ ] 1 操作で履歴が最新位置へ戻る。
- [ ] 入力欄の Home / End / Delete の挙動を変えない。
- [ ] 最新位置にいるときだけ新着に追従する。
- [ ] `document/11-keybindings.md` と `document/03-tui.md` を更新する。
