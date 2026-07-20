---
number: 392
title: fix(daemon): root-scoped Codex decision owner を session 非依存にする
status: done
priority: high
labels: [daemon, mcp, codex, security]
dependson: []
related: [383, 378, 379]
created_at: 2026-07-20T03:55:43.501120+00:00
updated_at: 2026-07-20T04:00:25.882916+00:00
---

## 背景

#383 は daemon-managed Codex の root / session / nested session で `user_decision_request` を runtime-fenced credential により認可する受け入れ条件を持つ。しかし `UserDecisionOwner.session_id` と `dispatch_user_decision` は session scope を必須にしており、root-scoped daemon Agent は有効な credential と binding を持っていても `ownership_unknown: decision worker has no session scope` で拒否される。

一方、daemon 外の root Codex / 手動 `usagi mcp` には provisioned credential がないため、引き続き fail-closed である。payload や cwd 等から root ownership を推測してはならない。

## やること

- decision owner の session scope を optional にし、daemon-minted credential → live runtime → matching dispatch binding の検証を通過した root scope を保存可能にする。
- session / nested session の既存 owner fence を保ち、credential 無し・forged・stale context は durable state を変更せず拒否する。
- root scope 成功、session scope 成功、credential 無し root MCP 拒否をテストで固定する。
- v2 正本の daemon / MCP 文書に、provisioned daemon-managed Codex だけが user decision を要求でき、daemon 外の手動 MCP は拒否される実行境界を記載する。

## 受け入れ条件

- daemon が起動した root-scoped Codex は、live credential と一致する dispatch binding を通じて decision を一度だけ作成できる。
- session ID が無いことだけを理由に拒否しない。
- caller context 無し・forged・runtime exit 後の context は fail-closed で、decision/inbox/durable state を変更しない。
- user supplied session/path/agent identity を authorization 入力にしない。
