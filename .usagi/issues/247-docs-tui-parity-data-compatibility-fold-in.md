---
number: 247
title: docs(tui): parity data compatibility と実装済み仕様の fold-in を行う
status: todo
priority: medium
labels: [tui, docs, parity]
dependson: [234, 235, 236, 238, 245, 246]
related: []
parent: 227
created_at: 2026-07-12T21:12:34.288369+00:00
updated_at: 2026-07-12T21:12:58.076016+00:00
---

## 目的

TUI-local resume data の compatibility/migration を確認し、実装済み parity 挙動を proposal から v2 仕様へ畳み込む。

## スコープ

- saved `TerminalRef`/target state の missing/stale/old data fallback と migration test。
- 実装済みの画面仕様、proposal current/gap の更新または stub 化、docs link check。

## 対象外

- 未実装 B/C を仕様として記載すること、daemon/core data schema の所有権変更。

## Acceptance ID

- release quality: data compatibility / docs fold-in。

## 依存

- A adapter/runtime と quality suite（#234/#235/#236/#238/#245/#246）。

## 検証

- compatibility fixture と `lychee` link check。
