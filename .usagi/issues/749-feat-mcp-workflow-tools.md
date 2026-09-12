---
number: 749
title: feat(mcp): workflow の起動と観測を MCP tool として公開する
status: done
priority: high
labels: [v2, cli, mcp, workflow]
dependson: []
related: [750]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T12:00:00+00:00
---

## 問題

`crates/cli` には workflow の語彙が 1 つも無い。workflow は TUI 専用で、root Agent や外部から
起動・観測できない。そのため usagi 本来の「issue を選んで委譲する」ループに乗らない。

## 方針

- `workflow_start` / `workflow_status` / `workflow_instruct` を MCP tool として公開する。
- **専用の workflow request（human-only）ではなく、既存の session tool と同じ経路に載せる**。
  `SessionAction` に 3 つの action を足し、名前解決・所有権（caller が作成した session に限る）・
  credential 検証・冪等性という既存の契約をそのまま継承する。TUI が使う
  `DaemonRequest::WorkflowSnapshot` / `WorkflowControl` の human-only gate は変更しない。
- 自己駆動の禁止は「caller 自身が動いている session を対象にできない」という形で表現する。workflow の
  担当 Agent が自分の workflow を操作できないという不変条件はこれで保たれる。
- 開始と指示は request の operation ID で冪等に受理する（retry が二重の run や指示を作らない）。
- status は phase・担当・修正回数・待ち理由・PR を含む snapshot をそのまま返す。

## 受入条件

- [x] MCP から workflow を起動でき、同じ operation ID の retry で二重起動しない。
- [x] MCP から phase・担当・修正回数・待ち理由・PR を取得できる。
- [x] caller 自身の session に対する操作が拒否される。
- [x] caller が作成していない session に対する操作が既存の所有権規則で拒否される。
- [x] `document/07-mcp.md` を更新する（IPC の語彙は session action なので `04-ipc.md` の変更は不要）。
