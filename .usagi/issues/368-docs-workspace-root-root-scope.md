---
number: 368
title: docs(workspace-root): 正本ドキュメントを root scope 契約に更新する
status: todo
priority: medium
labels: [docs, workspace-root]
dependson: [364, 365, 366, 367]
related: []
parent: 363
created_at: 2026-07-19T21:05:36.093588+00:00
updated_at: 2026-07-19T21:05:36.093588+00:00
---

## 目的

root Agent/Terminal の実装契約に合わせて正本ドキュメントを更新する。

## 変更内容

- `document/proposals/10-workspace-root-scope.md`（新規・設計正本）: durable root scope/ownership/fencing、trusted root path、restart/reconnect、pane projection、live input/detach、security invariants。
- `document/05-daemon.md`: root scope 解決（trusted repository root）と ownership/fence の記述を追加。
- `document/04-ipc.md`: `session_id: Option` の scope/ref/fence 語彙と root の意味。
- `document/03-tui.md`: `Target::Root` の pane projection と live IO。
- `document/proposals/README.md` の目次追記。

## 完了条件

- 記載＝実装済み・SSoT・相対リンク／アンカー整合（Markdown link check green）。

## 依存

Epic #363（実装 issue 群と同一 PR）。
