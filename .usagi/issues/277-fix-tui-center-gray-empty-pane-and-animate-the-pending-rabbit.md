---
number: 277
title: fix(tui): center gray empty pane and animate the pending rabbit
status: done
priority: high
labels: [tui, parity]
dependson: []
related: [276]
created_at: 2026-07-13T02:40:03.127242+00:00
updated_at: 2026-07-13T02:40:06.770551+00:00
---

## 目的

右ペイン空状態を v1 parity に揃え、pending tab では色付きのうさぎが chip 内を走る frame 表示にする。

## 実装

- 空 pane の rabbit、案内、safe feedback を右ペイン中央へ置く。
- rabbit と caption は灰色で描き、clip 後の style 適用と ANSI reset で狭幅でも後続へ色を漏らさない。
- pending chip は `🐇` だけを着色し、frame ごとに chip 内を進める。label 全体は dim のままにする。
- pending → live/failure の stable identity reducer は表示 animation によって変更しない。

## 検証

- 中央配置、灰色 ANSI reset、狭幅 clipping、running rabbit frame を widget/view test で検証する。
