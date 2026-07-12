---
number: 233
title: feat(tui): agent phase と safe feedback projection を実装する
status: todo
priority: high
labels: [tui, feedback]
dependson: [223, 225]
related: []
parent: 227
created_at: 2026-07-12T21:11:18.431701+00:00
updated_at: 2026-07-12T21:11:18.431701+00:00
---

## 目的

agent phase 集約と progress/operation/terminal/connection feedback を TUI-local projection と固定領域描画へ実装する。

## スコープ

- runtime ごとの phase 更新と done > waiting > running > ready > absent 集約。
- safe message と error_id のみを表示する progress/error/disconnect state。

## 対象外

- daemon phase/event wire、再接続実装、secret を含む error detail の生成。

## Acceptance ID

- `A-PHASE-1` / `A-FEEDBACK-1` の pure/fake slice。

## 依存

- #223/#225。D2/D3/D5/D6 adapter integration は #234。

## 検証

- reducer/render test で runtime isolation、rank、safe error redaction を確認する。
