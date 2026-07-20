---
number: 395
title: fix(tui): Switch 非選択 session の相対時刻を dim に保つ
status: done
priority: medium
labels: [tui]
dependson: []
related: []
created_at: 2026-07-20T03:58:52.249521+00:00
updated_at: 2026-07-20T04:28:32.321882+00:00
---

## 目的

Switch sidebar で以前選択されていた session が非選択になった後も、補足行の相対時刻が ANSI reset により dim を失わないようにする。

## 完了条件

- Switch の非選択 session 補足行（相対時刻、PR、Git summary）が ANSI span を含んでも dim を維持する。
- 名前・marker と Git/PR の既存の意味色・clip/pad 契約を維持する。
- 回帰テストと `document/03-tui.md` の正本契約を更新する。
