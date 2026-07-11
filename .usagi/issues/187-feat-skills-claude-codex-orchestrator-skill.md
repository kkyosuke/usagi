---
number: 187
title: feat(skills): Claude/Codex 共通の issue orchestrator skill を配布する
status: todo
priority: high
labels: [orchestration, agent, skills]
dependson: [183]
related: [99, 146]
parent: 182
created_at: 2026-07-11T00:30:00+00:00
updated_at: 2026-07-11T00:30:00+00:00
---

## 背景

現行の bundled skill は `<data-dir>/skills` に materialize した同一 directory を session worktree の `.claude/skills` へ symlink するため Claude Code では利用できる。一方、Codex の repo skill discovery path である `.agents/skills` には配布しておらず、`agent_cli: codex` で起動した worker/coordinator に同じ workflow を確実に渡せない。

Claude Code と Codex は `SKILL.md` の `name` / `description` と Markdown 本文を共有できる。Claude 固有 frontmatter や commands 形式へ寄せず、durable state と判定は #183 以降の usagi API に置く。

## やること

- `assets/skills/usagi-orchestrate-issues/SKILL.md` を追加する。
  - coordinator の plan 作成/再開、reconcile/action/ack の反復
  - worker の issue 着手、PR/result 報告
  - main merge を merge-ready とし、work-ready の stacked 作業と区別する安全規約
  - timeout/CI/review/conflict は coordinator が返す policy に従い、会話内で retry count を発明しない規約
- embedded skill の同一 materialized directory を、各 worktree の `.claude/skills/<name>` と `.agents/skills/<name>` の両方へ symlink する。
- workspace root coordinator と session の作成・復旧経路で link を保証する。
- 両 path を worktree-local git exclude に追加し、project が所有する同名の実 directory/file は上書きしない。
- durable tool 未実装/無効時は、既存 `issue_*` / `session_delegate_issue` / `session_status` / `session_pr` / `session_prompt` を使う best-effort mode に明示的に縮退する。
- Claude/Codex それぞれを選んだ session で skill discovery と明示 invocation を smoke test する。

## 受け入れ条件

- 同一 `SKILL.md` が Claude Code と Codex の双方から発見され、本文の二重管理がない。
- `session_delegate_issue(number, agent_cli, model)` で選んだ CLI/model が維持され、その worker が skill と usagi MCP を利用できる。
- coordinator/worker のどちらを再起動しても skill が再配布され、project 所有 skill を破壊しない。
- skill は durable state、claim、attempt、deadline の正本にならず、#183 の reconcile 結果に従う。
- materialize/link/exclude/衝突/disabled feature の unit test と Claude/Codex launch integration test がある。
- `document/04-orchestration.md`、`document/05-settings.md`、該当 data/command 文書を実装確定後に更新する。

## 非目標

- `.claude/commands` の新規追加。
- skill の prompt だけで exactly-once、timeout、CI/review polling を実現すること。
- Claude 固有 subagent frontmatter や Codex plugin packaging を共通 workflow の必須条件にすること。
