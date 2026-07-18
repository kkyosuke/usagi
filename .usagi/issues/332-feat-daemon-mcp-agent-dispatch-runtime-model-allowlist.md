---
number: 332
title: "feat(daemon): MCP agent dispatch で runtime/model allowlist を再検証する"
status: done
priority: high
labels: [daemon, mcp, agent, config]
dependson: [322, 331]
related: [323]
parent: 105
created_at: 2026-07-18T00:30:00+00:00
updated_at: 2026-07-17T23:42:17.208879+00:00
---

## 目的

`session_dispatch` の schema snapshot が古くなっていても、daemon が launch 前に current workspace allowlist と executable availability を再検証する。設計の正本は [document/proposals/08-agent-dispatch-mcp.md](../../document/proposals/08-agent-dispatch-mcp.md#9-runtimemodel-allowlist-schema-snapshot-と再検証) である。

## スコープ

- #322 の daemon dispatch launch 経路で current workspace 設定を読み、runtime/model が allowlist にあることを検証する。
- current executable locator で CLI availability を再検証し、CLI 不在を safe unavailable、未許可・不完全・混在入力を invalid argument として spawn 前に拒否する。
- `agent.id` branch は runtime/model と排他的にし、既存 agent の authorization/lifecycle scope を維持する。
- MCP から path、argv、environment、credential、CLI raw output、provider model list を受け取る・保存する経路を作らない。

## 完了条件

- fixture executable の追加・削除、allowlist 変更、未許可 model、id 混在を dispatch 実行時に決定的に検証できる。
- schema 発行後に CLI 削除または allowlist 縮小をしたケースを、spawn 前に拒否する integration test がある。
- accepted launch は managed session の完全な identity scope を通り、raw CLI output・credential・provider model list を wire / durable record に含めない。
