---
number: 232
title: feat(tui): terminal と agent tab の pending pane reducer を実装する
status: todo
priority: high
labels: [tui, pane]
dependson: [223, 224, 226]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.347588+00:00
updated_at: 2026-07-12T21:11:18.347588+00:00
---

## 目的

Closeup 内の terminal/agent tab、resolving/starting placeholder、exit と選択維持を fake reducer で固定する。

## スコープ

- stable `TerminalRef` を持つ live/pending tab model、success/failure/exit transition。
- requested target/pending tab が選択中の場合だけ attach する local policy。

## 対象外

- daemon inventory/stream attach、PTY input/output、永続 resume adapter。

## Acceptance ID

- `A-PANE-1` の pure/fake slice。

## 依存

- #223/#224/#226。D1/D3/D4/D6 実結合は #233。

## 検証

- reducer scenario で placeholder、reuse、exit、background 維持を確認する。
