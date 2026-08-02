---
number: 619
title: feat: session role を end-to-end で実装する
status: done
priority: high
labels: [session, role, daemon, mcp, tui]
dependson: []
related: [620]
created_at: 2026-08-01T00:21:30.701345+00:00
updated_at: 2026-08-01T01:46:51.710219+00:00
---

## 目的

`document/proposals/14-session-roles.md` を設計正本として session role を実装し、domain / daemon / MCP / CLI / documentation を一貫させる。

## 実装スコープ

- versioned global/workspace role catalog、検証、deterministic merge
- stable role assignment と legacy serde compatibility
- create / dispatch / delegate の selector、default、idempotency / conflict
- registered workspace root での daemon 再検証
- scope + role + local-LLM prompt の一度だけの合成と ephemeral adapter injection
- instruction 非永続化、safe metadata projection
- MCP schema / orchestration guide / CLI `--role`
- proposal acceptance criteria の unit / integration tests

## TUI の依存分離

現行 TUI は daemon lifecycle row を永続 UI 注釈 `SessionRecord` へ落として sidebar を構築する。ここへ role を足すと assignment が daemon `sessions.json` と UI `state.json` の二重正本になるため、安全な一PR完遂には daemon role metadata を UI-only projection として controller へ運ぶ seam が先に必要である。editor・create picker・badge はこの依存順と受け入れ条件を #620 に起票した。時間都合ではなく assignment SSoT を守るための分離である。

## 受け入れ条件

proposal 第8節の domain/daemon/MCP/CLI 契約を満たし、role が sandbox/guard を弱めず、リスク比例 gate と PR CI が green になること。
