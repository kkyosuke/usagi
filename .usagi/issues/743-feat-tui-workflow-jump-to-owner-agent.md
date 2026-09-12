---
number: 743
title: feat(tui): Workflow タブから現在の担当 Agent タブへ移動する
status: todo
priority: medium
labels: [v2, tui, workflow, agent]
dependson: []
related: []
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

Workflow タブは進行の要約しか出さない。生の出力を見るには Ctrl-O で session を辿り、どの Agent タブが
実装担当でどれがレビュー担当かを自分で見分ける必要がある。run は担当の `AgentId` を保持しているので、
この手間は不要である。

## 方針

- Workflow タブから 1 キーで「現在の phase の担当 Agent」のタブへ移動する操作を追加する。
- 担当が停止している場合は移動せず、理由を出す。
- キー割当は既存の workflow 入力（Enter / Tab / Ctrl-S / PageUp / PageDown）と衝突しないものを選ぶ。
- 実装担当・レビュー担当を明示的に選んで移動できてもよい。

## 受入条件

- [ ] 現在の担当の Agent タブへ 1 操作で移動できる。
- [ ] 担当が停止している場合は理由を出して移動しない。
- [ ] 既存の workflow 入力のキー割当を壊さない。
- [ ] `document/11-keybindings.md` と `document/03-tui.md` を更新する。
