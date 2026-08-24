---
number: 718
title: effective MCP tool 系統の解決を 1 か所に統合する
status: done
priority: medium
labels: []
dependson: []
related: [715]
created_at: 2026-08-24T00:15:58.687629+00:00
updated_at: 2026-08-24T00:31:21.868029+00:00
---

## 背景

「Global 設定に workspace の `.usagi/settings.json` を重ねて effective な issue / memory の可否を出す」規則が、2 か所に別々に実装されている。

| 実装 | 用途 | 型 |
|---|---|---|
| `crates/cli/src/mcp/serve.rs` → `ToolAvailability::from(&global.with_local(&local))` | MCP tool registry の組み立て | `ToolAvailability { issue, memory }` |
| `src/runtime/daemon.rs` の `configured_mcp_tools` | Agent 起動 prompt の `<tools>` fragment | `McpToolFamilies { issue, memory, local_llm }` |

両者は同じ 2 層を同じ順で畳んでおり、今は一致している（#715 で E2E `production_disabled_family_leaves_both_the_registry_and_the_agent_prompt` が一致を固定した）。しかし規則そのものは 2 ファイルに複製されており、片方だけを変更しても compile は通る。prompt と `tools/list` が食い違うのは #715 が直した不具合そのものなので、規則を 1 か所に寄せて構造的に再発しないようにする。

型も構造的に重複している（`ToolAvailability` は `McpToolFamilies` の issue / memory 部分と同義）。

## やること

- effective な tool 系統を返す関数を `usagi-core` に 1 つ置く（例: `usecase::tool_families::effective(global, local) -> McpToolFamilies`）。設定の読み取り（どの root を権威にするか）は呼び出し側の責務のまま残す。MCP server は cwd / trusted root、daemon は登録済み workspace root を渡す。この違いは意図的なので統合しない。
- `crates/cli` の `ToolAvailability` を core の型から導出するか置き換える。registry の filter は `McpToolFamilies` を受け取る。
- `src/runtime/daemon.rs` の `configured_mcp_tools` は読み取りと fail-closed だけを持ち、畳み込みは core の関数へ委譲する。
- `local_llm` の権威は Global のみという規則（`with_local` が持たない field）を core 側の doc comment に明記する。
- 既存の E2E（registry と prompt の一致）は維持する。

## 確認方法

- `cargo test -p usagi-core` / `cargo test -p usagi-cli` / `cargo test -p usagi --bin usagi`
- `cargo test -p usagi --test mcp_e2e`（`production_disabled_family_leaves_both_the_registry_and_the_agent_prompt` が引き続き通ること）
