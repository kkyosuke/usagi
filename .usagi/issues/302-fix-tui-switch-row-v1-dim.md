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
updated_at: 2026-07-14T23:15:50.026508+00:00
---

## 目的

Switch mode の左 sidebar で、選択中の session と常設の `+ new session` 以外を v1 と同じ dim の非アクティブ色で描画し、cursor/current の既存契約を保つ。

## 受け入れ条件

- Switch では cursor がない root / session 行の label が dim で描画される。
- 選択中の session は既存の Accent 太字、`+ new session` は既存の Success 表示を維持する。
- current marker、stable identity による selected/active 照合、Closeup の表示は変えない。
- renderer regression test と `document/03-tui.md` を更新する。
