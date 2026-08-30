---
number: 727
title: "perf(tui): 連続ホイール入力を描画前に集約する"
status: done
priority: high
labels: [tui, performance, scroll, input]
dependson: []
related: [637, 660]
created_at: 2026-08-29T00:00:00+09:00
updated_at: 2026-08-29T00:00:00+09:00
---

## 問題

Crossterm の trackpad / mouse wheel event は `EventPump::next` から 1 件ずつ Home loop へ返る。
Home loop は各 event の前に terminal viewport projection、Home material、frame diff、stdout flush を行うため、
連続 wheel input が入力 queue に溜まると 1 notch = 1 frame の処理になり、scroll が遅れて重く感じる。
terminal event は tick より優先されるため、burst 中は backlog も解消しにくい。

## 修正方針

- ready 済みの同方向 wheel event を描画前に bounded coalesce する。
- wheel の座標は最新 event を使い、集約した notch 数を TUI-local action に運ぶ。
- wheel 以外の最初の event は順序を変えず次回へ保持する。
- mouse protocol / alternate screen への転送と primary scrollback の移動量は集約前と一致させる。
- 無限に入力を drain して描画を飢餓させない上限を設ける。

## 受入条件

- 同方向 wheel burst が 1 action / 1 redraw に集約される。
- 逆方向 wheel、key、resize の順序は維持される。
- primary / alternate / mouse-protocol の移動量が従来と一致する。
- burst coalesce は bounded で、継続入力中も frame が更新される。
