---
number: 120
title: refactor(agent): model_flag_parts の三重定義と MCP サーバ台帳・agent-phase 語彙を SSoT 化する
status: todo
priority: medium
labels: [refactor, agent, review]
dependson: []
related: [119]
created_at: 2026-07-04T23:14:45.302134+00:00
updated_at: 2026-07-04T23:14:45.302134+00:00
---

## 背景（なぜ問題か）

agent アダプタ群に、フラグ文字列以外がバイト一致の関数・台帳が散在している。

1. **model_flag_parts の三重定義**: `codex.rs`/`gemini.rs`/`antigravity.rs` の `model_flag_parts` はフラグ文字列以外バイト一致で三重定義（#47 は trait 二重間接の話で、この関数重複は別）。
2. **MCP サーバ台帳の二重定義**: `usagi`={bin, ["mcp"]} と条件付き `usagi-llm`={bin, ["llm-mcp", "--model", model]} の台帳が `claude.rs`（serde struct）と `codex.rs`（TOML override）で二重定義され、**codex.rs 内では `wiring_overrides` と `headless_command` でさらに重複**している（claude は `mcp_config_json` を再利用して回避済み）。
3. **agent-phase 語彙**: phase 語彙（`ready`/`running`/`waiting`/`ended`）が `claude.rs`・`codex.rs`・`main.rs`・`cli/agent_phase.rs` に散在している。

## 対象箇所

- `agent/{codex,gemini,antigravity}.rs::model_flag_parts`
- `claude.rs::{mcp_config_json, claude_hooks_settings}`
- `codex.rs::{mcp_server_overrides, wiring_overrides, hook_override, headless_command}`
- `HOOK_PHASES` 相当・`main.rs`・`cli/agent_phase.rs` の phase verb

## やること

- `model_flag_parts(wiring, flag)` を `agent/util.rs`（仮）へ集約する。
- MCP サーバ台帳を `AgentWiring` 由来の中立記述（`Vec<(name, cmd, args)>`）にして各エンコーダで描画する。まず codex.rs 内の自己重複解消だけでも効果大。
- phase 語彙と `agent-phase` verb を共有定数化する（producer=アダプタ / consumer=CLI を結ぶ）。

## 受け入れ条件

- `usagi`/`usagi-llm`/`mcp`/`llm-mcp`/phase 語が単一定義になり、生成される JSON/TOML が現状不変（テストで固定）。
- 既存テストが緑、カバレッジ 100% 維持。
