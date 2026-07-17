---
number: 311
title: fix(daemon): runtime snapshot を durable atomic JSON writer で永続化する
status: done
priority: high
labels: [daemon, persistence]
dependson: []
related: [268]
created_at: 2026-07-17T11:10:56.575393+00:00
updated_at: 2026-07-17T11:17:23.960506+00:00
---

## 背景

daemon-owned `terminals.json` / `agents.json` は durable runtime snapshot だが、root composition の `FileTerminalStore` / `FileRuntimeStore` が固定 `*.json.tmp` への write と rename を独自実装している。temp file の fsync、rename 後の parent directory fsync、writer ごとの一意 temp 名、失敗時の cleanup がなく、core の durable JSON writer 契約と不整合である。

## 完了条件

- 両ストアが core の durable atomic JSON writer（または同等の安全性を持つ共有 helper）で snapshot を保存する。
- 保存失敗時に既存 snapshot を置換せず、temp artifact を残さないことを回帰テストで確認する。
- `terminals.json` / `agents.json` の durable guarantee を daemon 正本ドキュメントへ正確に記載する。
- Rust / Markdown の該当品質 gate を通す。
