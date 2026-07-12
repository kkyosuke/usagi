---
number: 237
title: feat(tui): detach-safe quit と Ctrl-C grace reducer を実装する
status: done
priority: high
labels: [tui, quit]
dependson: [223, 224, 232]
related: []
parent: 227
created_at: 2026-07-12T21:11:47.719622+00:00
updated_at: 2026-07-12T22:52:41.580097+00:00
---

## 目的

live/management/modal ごとの Ctrl-C/Ctrl-Q を安全に分類し、daemon を停止せず client detach を行う。

## スコープ

- live Ctrl-C passthrough、management confirmation、modal inert、yes/no、one-shot grace。
- effect を detach request に限定する controller policy。

## 対象外

- daemon の terminal/operation cancel、PTY byte encoder の再実装、実 socket detach adapter。

## Acceptance ID

- `A-QUIT-1` の reducer slice。

## 依存

- #223/#224/#232。D6 transport connection は #220 client adapter を利用する後続 runtime compositionで結合する。

## 検証

- reducer + PTY classifier scenario（grace、modal、live、detach）を追加する。
