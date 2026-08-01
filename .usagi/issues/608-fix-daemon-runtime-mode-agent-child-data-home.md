---
number: 608
title: fix(daemon): runtime mode に応じて Agent child の data home を導出する
status: in-progress
priority: high
labels: [review, v2, daemon, agent, config, security, correctness]
dependson: []
related: [512, 537, 542]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-07-31T23:41:14.184775+00:00
---

## Finding（P1 correctness/security）

`src/runtime/daemon.rs::open_agent_runtime` は selected `data_dir` の `parent()` を常に `data_home` とする。`crates/core/src/infrastructure/paths.rs::mode_data_dir` では production の `data_dir` は base 自体、local/development だけが `base/local|dev` なので、production では誤って base の親を選ぶ。この値が `mcp_environment` の child `USAGI_HOME`、`configured_local_llm_model`、`claude_writable_roots` に流れ、別 settings を読む、MCP が別 state tree を選ぶ、sandbox writable scope を一階層広げる。

## 最小修正方針

runtime mode と base/selected directory の関係を `paths` の typed helper で一元化し、production は `data_dir`、local/development のみ parent を base とする。呼出側で path shape を推測しない。

## テストと受け入れ条件

- 3 mode × custom `USAGI_HOME` で child env、local LLM settings source、Claude writable roots が同じ intended base を指す。
- production で base の親が writable root / settings source にならない。
- child が同じ runtime mode を再適用して daemon の selected directoryへ戻る契約を E2E で固定する。
