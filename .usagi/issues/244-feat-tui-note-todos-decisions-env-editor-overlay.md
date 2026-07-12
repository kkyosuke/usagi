---
number: 244
title: feat(tui): note・todos・decisions と env editor overlay を実装する
status: done
priority: medium
labels: [tui, parity-b, overlay]
dependson: [226, 228]
related: []
parent: 227
created_at: 2026-07-12T21:12:34.018319+00:00
updated_at: 2026-07-12T22:58:16.219014+00:00
---

## 目的

Session note/todos/decisions と workspace/session env editor を overlay として提供する。

## スコープ

- note/todos/decisions の表示/編集 state、env read/edit/save、safe error。

## 対象外

- bulk remove checklist、note chord、backend persistence schema の変更。

## Acceptance ID

- `B-OVERLAY-2`（proposal の note/todos/decisions/env 後回し項目）。

## 依存

- #226、#228。各 persistence/settings port の所有権は維持する。

## 検証

- fake port scenario と overlay render で edit/save/failure/background preservation を確認する。
