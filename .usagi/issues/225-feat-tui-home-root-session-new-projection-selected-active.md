---
number: 225
title: feat(tui): Home root/session/+new projection と selected/active 独立描画
status: done
priority: high
labels: []
dependson: [223, 224]
related: []
created_at: 2026-07-12T13:24:53.585668+00:00
updated_at: 2026-07-12T13:29:32.066866+00:00
---

A-HOME-1 を実装する。

- AppState の typed Target / Selection を Home 描画へ投影する
- root → sessions → + new session を常設し、selected と active を独立 marker として同時描画する
- + new は active target にせず、snapshot 消失時は root へ fallback する
- pure model + render tests で stable identity を固定する。
