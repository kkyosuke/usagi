---
number: 60
title: refactor(presentation): CLI/MCP の JSON 整形を SSoT 化し mcp 共通ヘルパを統合する
status: todo
priority: medium
labels: [refactor, cli, review]
dependson: []
related: []
created_at: 2026-06-19T22:16:42.363888+00:00
updated_at: 2026-06-19T22:16:42.363888+00:00
---

## 背景

CLI と MCP が同一ドメイン型に対して別々の JSON 整形コードを持ち、保守時にフィールド追加が片方へ漏れるリスクがある。

### 1. 同一型の JSON 整形が 2 系統（保守性リスク・中）
- Issue: `src/presentation/cli/issue/render.rs:104-154`（serde derive の `IssueJson`/`ListItemJson`）と `src/presentation/mcp/issue/json.rs:9-50`（`json!` 手組みの `issue_to_json`/`listed_to_json`）が**同じフィールド集合**を別コードで生成。
- Memory: `src/presentation/cli/memory/render.rs:36-72` と `src/presentation/mcp/memory.rs:171-200` も同様。
→ `domain` か `usecase` に「issue→serde 表現」を 1 箇所定義し、CLI/MCP 双方が `serde_json::to_value` / `to_string_pretty` で消費する形に寄せる（06-conventions の SSoT 規約に合致）。

### 2. mcp の小ヘルパ重複（低）
- `to_pretty`: `mcp/issue/json.rs:52`・`mcp/memory.rs:202`・`mcp/session.rs:158` に同一定義 3 つ。
- `parse_args`: `mcp/issue/mod.rs:279`・`mcp/memory.rs:165`・`mcp/session.rs:130` に 3 重複。
→ `mcp/mod.rs` の共通ヘルパへ集約。

### 3. stdio serve ループ重複（中）
`cli/mcp.rs:78-92` と `cli/llm_mcp.rs:82-100` がほぼ同一（行読み→空行スキップ→`handle_line`→write+flush）。`llm_mcp.rs` 側は generic 化済みなので、共通 `serve(reader, writer, server)` ヘルパへ `mcp.rs` を合流させる。

## 確認方法

- フィールド追加が 1 箇所の変更で CLI/MCP 双方に反映されること。
- 既存の JSON 出力が変わらないこと（既存テスト維持、カバレッジ 100%）。
