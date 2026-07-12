---
number: 230
title: feat(tui): fake attach flow で Welcome/Open/Recent から Home を接続する
status: todo
priority: high
labels: [tui, entry]
dependson: [223, 225]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.202008+00:00
updated_at: 2026-07-12T21:11:18.202008+00:00
---

## 目的

fake backend を使い Welcome→Open/Recent→Home の identity-preserving flow と Open error retry を controller に実装する。

## スコープ

- Open Single 選択、Recent 選択、空/stale/error の画面内状態と retry。
- Home snapshot の typed identity による初期化。

## 対象外

- Unix socket/daemon attach、alternate screen の実端末復元、Open filter/Unite。

## Acceptance ID

- `A-ENTRY-1` / `A-ENTRY-2` の fake backend slice。

## 依存

- #223、#225。D1 実結合は #231 が #220 に依存して行う。

## 検証

- fake backend reducer/runtime scenario で選択 identity、stale/error/retry を確認する。
