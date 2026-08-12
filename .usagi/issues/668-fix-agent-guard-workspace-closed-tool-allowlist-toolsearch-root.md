---
number: 668
title: fix(agent): guard-workspace の closed tool allowlist が ToolSearch を塞ぎ root から委譲できない
status: done
priority: high
labels: [fix, agent, guard]
dependson: []
related: []
created_at: 2026-08-12T05:02:59.198445+00:00
updated_at: 2026-08-12T05:09:42.456513+00:00
---

## 症状

workspace root（コーディネータ／director 行）の Claude Code から usagi の MCP tool が一切呼べない。
`mcp__usagi__*` は deferred tool として提示されるため schema を取るには `ToolSearch` が要るが、その
`ToolSearch` が

```
unknown tool is denied fail-closed: ToolSearch
```

で拒否される。結果として root は `session_delegate_brief` / `session_delegate_issue` に到達できず、
CLAUDE.md が root に指示している「issue を書かず session へ委譲する」経路そのものが塞がる。root は
`issue_create` を（設計どおり）拒否されるため、起票も委譲もできない鶏と卵になる。

## 再現

1. workspace root で Claude Code を起動する（`.usagi/sessions/` 配下でない cwd）。
2. `mcp__usagi__session_delegate_brief` を呼ぶ → schema 未ロードで失敗。
3. `ToolSearch` を呼ぶ → 上記の deny。

MCP server 自体は正常に接続しており（`mcp-logs-usagi` に `Successfully connected`）、
`mcp__` prefix の tool は guard を通る。塞いでいるのは唯一の入口である `ToolSearch` である。

## 原因

`crates/cli/src/cli/hooks/guard_workspace.rs` の session / root 両モードが、非書き込みツールを
**ツール名の closed allowlist** で判定し、外れたものを fail-closed で拒否していた。

```rust
"Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "Task" | "Skill" | "TodoWrite"
| "AskUserQuestion" => None,
name if name.starts_with("mcp__") => None,
_ => Some(format!("unknown tool is denied fail-closed: {tool_name}")),
```

この allowlist は harness の tool inventory から drift しており、`ToolSearch` のほか `Agent`
（`Task` から改名）、task 系（`TaskCreate` / `TaskUpdate` …）、`EnterPlanMode` / `ExitPlanMode`、
`Monitor` / `SendMessage` / `Workflow` などが session モードでも一律に拒否されていた。
guard が守る性質（worktree 外への書き込みと root の repository mutation を止める）は
`is_write_tool` / `command_mutates_repo` が担っており、名前を知らないことは変更能力の証拠ではない。

副次的な不具合として、`NotebookEdit` は `notebook_path` を使うのに shim が `file_path` しか見て
いないため、session モードで常に `payload has no file_path` で拒否されていた。

## 修正方針

判定を「未知の名前は拒否」から「**変更能力の shape で判定**」へ変える。

- `usagi-core` の `usecase::workspace_guard` に `ToolGuard`（`FileWrite` / `Shell` / `Unrestricted`）と
  `classify_tool(tool_name, input_keys)` を置く。
  - 既知の書き込みツール → `FileWrite`、`Bash` → `Shell`、`mcp__*` と既知の非変更ツール → `Unrestricted`。
  - 未知でも `tool_input` が file を名指しする key（`file_path` / `notebook_path` / `path`）を持てば
    `FileWrite` として fail-closed に倒す。持たなければ通す。
- shim は分類に従い、session では書き込み先候補すべてを worktree へ閉じ込め、root では `FileWrite` を
  パスによらず拒否する。

これで harness が tool を増やしても guard は壊れず、`ToolSearch` 経由の MCP 到達性が回復する。

## テスト観点

- root モードで `ToolSearch` / `Agent` / `TaskCreate` / `EnterPlanMode` が通ること。
- root モードで未知でも file を名指しするツールが拒否されること。
- session モードで未知ツールの `path` / `notebook_path` が worktree 外なら拒否、内側なら通ること。
- 既存の write tool・shell allowlist・malformed payload の挙動が変わらないこと。
