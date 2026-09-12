---
number: 749
title: feat(mcp): workflow の起動と観測を MCP tool として公開する
status: todo
priority: high
labels: [v2, cli, mcp, workflow]
dependson: []
related: [750]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`crates/cli` には workflow の語彙が 1 つも無い。workflow は TUI 専用で、root Agent や外部から
起動・観測できない。そのため usagi 本来の「issue を選んで委譲する」ループに乗らない。

## 方針

- `workflow_start` / `workflow_status`（必要なら `workflow_instruct`）を MCP tool として公開する。
- 既存の human-only 制約との整合を明示する。現在 daemon は `caller_context` 付き request を拒否して
  workflow control を human-only にしている。MCP 経由の起動をどう扱うかを設計で決める:
  - root の MCP client（利用者の代理）からの起動は許可し、**session 内 Agent からの自己起動は拒否**する、
    という境界を IPC 側で表現する。
- 書き込み系 tool は issue store と同じく root / session の扱いを明示する。
- status は phase・担当・修正回数・待ち理由・PR URL を返す。

## 受入条件

- [ ] MCP から workflow を起動でき、同じ operation ID の retry で二重起動しない。
- [ ] MCP から phase・担当・修正回数・待ち理由・PR を取得できる。
- [ ] session 内 Agent による自己起動・自己操作が拒否される。
- [ ] `document/07-mcp.md` と `document/04-ipc.md` を更新する。
