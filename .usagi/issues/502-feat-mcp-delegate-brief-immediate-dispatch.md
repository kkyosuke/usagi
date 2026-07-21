---
number: 502
title: feat(mcp): session_delegate_brief を即時 dispatch する
status: done
priority: high
labels: [mcp, orchestration]
dependson: [402]
related: [109, 323]
parent: 400
created_at: 2026-07-21T00:00:00+00:00
updated_at: 2026-07-21T01:00:00+00:00
---

`session_delegate_brief` は作成した triage session の prompt を queue に残さず、認証済み caller の
provenance を保持して worker を即時 dispatch する。

## 受入条件

- [x] brief は session 作成後、`session_dispatch` と同じ daemon-owned dispatch 経路で worker を起動する。
- [x] worker selector は `agent.id` または `agent.runtime` と `agent.model` の完全な組だけを一意に受理する。
- [x] credential/provenance、worktree 隔離、runtime/model allowlist、失敗時の typed error を維持する。
- [x] session delegate issue の queue/autostart 挙動と user decision tool は変更しない。
- [x] MCP schema・失敗系テスト・orchestration docs を同期する。
