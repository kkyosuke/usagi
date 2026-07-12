---
number: 238
title: feat(tui): runtime を alternate screen・event pump・frame diff で合成する
status: done
priority: high
labels: [tui, runtime]
dependson: [228, 229, 230, 220]
related: []
parent: 227
created_at: 2026-07-12T21:11:47.788110+00:00
updated_at: 2026-07-12T22:52:10.952498+00:00
---

## 目的

TUI runtime を alternate screen lifecycle、統一 event stream、frame-diff output、D1 client attach で合成する。

## スコープ

- Welcome/Open/Recent→Home の実 backend attach、raw mode/cursor/mouse/alternate screen 復元。
- renderer output と event pump を合成し resize reset を runtime で扱う。

## 対象外

- lifecycle/pane/phase の各 adapter、B surface。

## Acceptance ID

- `A-ENTRY-1` / `A-ENTRY-2` の D1 実結合、および `A-RENDER-1` の実端末 lifecycle 部分。

## 依存

- #228/#229/#230 と D1 を提供する #220。

## 検証

- fake backend runtime と real PTY で entry/quit/resize/terminal restore を確認する。
