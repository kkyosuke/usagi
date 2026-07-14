---
number: 302
title: fix(tui): Switch の非選択 row を v1 と同じ dim にする
status: done
priority: medium
labels: [tui, bug, parity, sidebar, switch]
dependson: []
related: [293]
parent: 227
created_at: 2026-07-14T23:13:58.206172+00:00
updated_at: 2026-07-14T23:52:17.743540+00:00
---

## 目的

実行中の v2 TUI sidebar を、参照画像どおり v1 と同じ色規約で描画する。

## 受け入れ条件

- Switch では選択中の session だけを Accent 太字にし、root・非選択 session・`+ new session` は dim にする。
- Closeup では root / session を Accent、current session を Accent 太字、`+ new session` を Success で描く。
- cursor/current marker、stable identity による selected/active 照合を変えない。
- runtime が実際に呼ぶ `workspace::render_with_skeleton_frame` をテストし、TUI 仕様を更新する。
