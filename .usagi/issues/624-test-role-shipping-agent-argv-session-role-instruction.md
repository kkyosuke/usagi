---
number: 624
title: test(role): shipping Agent argv で session role instruction を固定する
status: todo
priority: medium
labels: [review, v2, test, role, daemon, mcp, pty, e2e]
dependson: [619]
related: [620]
created_at: 2026-08-02T23:07:31.677514+00:00
updated_at: 2026-08-02T23:07:31.677514+00:00
---

## Finding（P2 / production 契約の未検証）

### path / symbol

- `tests/mcp_e2e.rs::production_delegate_brief_immediately_dispatches_an_isolated_triage_worker`
- `tests/mcp_e2e.rs::production_dispatch_worker_complete_reaches_the_caller_inbox`
- `src/runtime/daemon.rs::effective_role_instruction`
- `src/runtime/daemon.rs::claude_system_prompt_arguments`
- `src/runtime/daemon.rs::codex_system_prompt_arguments`

### 発生条件

role 付き `session_delegate_brief` / `session_dispatch` から shipping `usagi mcp`、実 daemon、実 PTY、fixture Agent を起動する。現行 E2E は role assignment、safe projection、dispatch 完了、instruction の durable store 非混入までは検証するが、fixture child が受け取った argv を記録・検査しない。

### 影響

production composition で role catalog の instruction が脱落、重複、scope safety prompt より前後逆転、Claude の `--append-system-prompt` または Codex/Sakana AI の `developer_instructions` へ未注入になっても、unit test と metadata E2E が green のままになる。#619 は「prompt の一度だけの合成と ephemeral adapter injection」を受入条件に含めて done だが、最重要の process/PTY 境界が unit/fake の保証に留まる。

### 具体的根拠

- `tests/mcp_e2e.rs` の role E2E は `DELEGATE_ROLE_SECRET` / `DISPATCH_ROLE_SECRET` が `dispatch.json` に無いことだけを assert し、spawn argv に存在することを assert しない。
- `src/runtime/daemon.rs::role_instruction_is_injected_once_for_claude_and_codex_without_entering_user_prompt` は純粋な argument builder の unit test であり、`effective_role_instruction → launch request → adapter → PTY child argv` の production wiring を通らない。
- `document/10-session-roles.md#prompt-合成` は scope safety prompt、role instruction、local-LLM suffix の順序と、adapter ごとの単一引数注入を契約としている。

### 修正方針

既存 shipping MCP/daemon/PTY fixture に argv capture を追加し、role 付き launch と explicit resume の child argv を process 境界で観測する。秘密値そのものを failure log に無制限に露出させず、構造・出現回数・順序を安全に検査する。

### 必須回帰テスト

- Claude: `--append-system-prompt` が1個で、scope safety → `<role id="...">` → optional suffix の順になる。
- Codex と Sakana AI: TOML として parse 可能な `developer_instructions` が1個で、同じ順序・1回性を満たす。
- user prompt、dispatch store、lifecycle/agent/run/binding store、MCP response に instruction が入らない。
- catalog definition 更新後の explicit resume は新 definition を使い、実行中 child は変更しない。
- role 無し legacy session の shipping argv は従来契約を維持する。

### docs / migration

仕様変更は不要。必要なら `document/10-session-roles.md` に production regression test 名だけを追記する。永続データ migration はない。
