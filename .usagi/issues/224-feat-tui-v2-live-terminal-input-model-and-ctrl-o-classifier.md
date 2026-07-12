---
number: 224
title: feat(tui): v2 live terminal input model and Ctrl-O classifier
status: done
priority: high
labels: [tui, input]
dependson: []
related: []
created_at: 2026-07-12T13:08:53.553790+00:00
updated_at: 2026-07-12T13:15:04.608136+00:00
---

## 目的

v2 TUI の live terminal 用に、端末非依存の入力語彙・PTY bytes encoder・Ctrl-O prefix classifier を実装し、A-INPUT-1 / A-INPUT-2 を daemon 非依存の純粋テストで固定する。

## 受け入れ条件

- key code / modifier / Press・Repeat・Release / UTF-8 text / paste / raw bytes を区別する。
- Ctrl-O leader（1 秒）、予約 action、unknown follow-up の one-shot swallow、Ctrl-^ を実装する。
- 非予約入力は順序と bytes を保って一度だけ passthrough し、Release は送らない。
- controller の旧 AppKey と live terminal 入力の責務を分離し、最小の接続のみを行う。
- public API の doc comment、architecture documentation、Rust quality gate を含める。
