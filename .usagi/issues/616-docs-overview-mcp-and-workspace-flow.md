---
number: 616
title: docs: overview の usagi mcp と New workspace の実装契約を同期する
status: todo
priority: medium
labels: [review, docs, mcp, tui, correctness]
dependson: []
related: [240, 341, 601]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-07-31T15:00:00+09:00
---

## Finding（P2 docs drift）

`document/01-overview.md` のコマンド表は `usagi mcp` が `usagi v<version> mcp ready` を表示すると記載するが、現行実装は daemon client を bootstrap して stdio JSON-RPC server を serve し、daemon unavailable は failure になる。同文書の Welcome/New workspace 説明も「作成処理が入るまで留まる」とするが、Clone/Existing は create/register 後 Workspace へ遷移し、failure時 draft retention まで実装済みである。

## 最小修正方針

overview を現行の user-visible contract に限定して更新し、MCP詳細は `document/07-mcp.md`、TUI詳細は `document/03-tui.md` を正本として相対リンクする。内部手順の重複記載は避ける。

## テストと受け入れ条件

- `usagi mcp` の成功条件、stdio JSON-RPC lifetime、daemon unavailable failure が実装と一致する。
- New workspace の Clone/Existing success transition と failure draft retention が現行 TUI と一致する。
- `rg` で obsolete ready-line / 未実装表現が正本文書に残らず、Markdown link check が通る。
