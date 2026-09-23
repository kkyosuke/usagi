---
number: 740
title: feat(tui): session 行に workflow の進行バッジを出す
status: todo
priority: medium
labels: [v2, tui, workflow]
dependson: []
related: [741]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

sidebar の session 行にも Garden にも workflow の痕跡が出ない。Closeup → `workflow` を開くまで、
その session に run があるかどうかも分からない。複数 session で回すと「どれが動いているか」を
覚えておく必要がある。

## 方針

- session 行に phase と修正回数の短いバッジを出す（例: `⟳ Reviewing 1/3`）。
- 幅が狭い場合の省略規則を既存のバッジ表示と揃える。
- 判断待ち（`Waiting`）は他と区別できる表示にする。
- バッジの元データは daemon の snapshot とし、TUI 側で phase を推測しない。
- run が無い session には何も出さない（既存の行の見た目を変えない）。

## 受入条件

- [ ] run のある session 行に phase と修正回数が出る。
- [ ] `Waiting` が視覚的に区別できる。
- [ ] 狭い幅で既存の行要素を押し出さない。
- [ ] run が無い session の行は変化しない。
- [ ] `document/03-tui.md` の sidebar 表示に追記する。
