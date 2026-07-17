---
number: 331
title: "feat(mcp): workspace allowlist から runtime/model schema snapshot を生成する"
status: todo
priority: high
labels: [mcp, agent, config]
dependson: [323]
related: [322, 332]
parent: 105
created_at: 2026-07-18T00:30:00+00:00
updated_at: 2026-07-18T00:30:00+00:00
---

## 目的

#323 が実装する `session_dispatch` の新規 agent branch を、workspace runtime/model allowlist と MCP 起動時の CLI availability snapshot によって厳密に制限する。設計の正本は [document/proposals/08-agent-dispatch-mcp.md](../../document/proposals/08-agent-dispatch-mcp.md#9-runtimemodel-allowlist-schema-snapshot-と再検証) である。

## スコープ

- workspace の `[agents.claude].models` と `[agents.codex].models` を runtime ごとの許可 model の正本として読む設定型・reader を追加する。
- `claude` / `codex` executable の PATH 探索を `ExecutableLocator` port に分離する。production は server 起動時に snapshot し、test は fake を注入する。
- `tools/list` の `session_dispatch` schema を、`agent.id` または runtime ごとの `runtime`/`model.enum` branch の `oneOf` にする。CLI 不在・空 allowlist の runtime は公開しない。
- `agent_cli` は既存 create/delegate tool の deprecated alias として parser で扱い、`runtime` または `agent.id` との混在を migration error として拒否する。未実装 tool に擬似 dispatch は追加しない。
- snapshot の再生成には MCP server の再起動または client 再接続が必要であることを、実装済み v2 正本へ記載する。

## 完了条件

- fake locator により Claude-only / Codex-only / none / 両方の schema を PATH 非依存で固定する。
- runtime ごとの model enum、未許可 model、空 allowlist、runtime/model 不完全組、id 混在を schema と parser の双方で検証する。
- listing 後の設定・PATH 変更は同一 server の schema を変えず、server 再生成時だけ変わることをテストする。
- CLI/provider の model list を取得・保存・allowlist 拡張しない。
