---
number: 145
title: refactor(agent): Gemini/Antigravity を shared no-inline adapter と resume store に分離する
status: todo
priority: medium
labels: [refactor, agent, review]
dependson: [139]
related: [119, 134]
parent: 137
created_at: 2026-07-06T00:21:43.618691+00:00
updated_at: 2026-07-06T00:21:43.618691+00:00
---

## 目的

Gemini / Antigravity の command builder 重複を `LaunchPlan` 土台へ移し、no-inline-MCP 系 adapter と resume/forget store を分離する。

## 背景

`gemini.rs` と `antigravity.rs` は、MCP/hook/system prompt を inline できないため opening prompt に worktree note を入れる、model flag を付ける、resume flag を付ける、headless prompt を作る、という構造がほぼ同じ。差分は program、permission bypass、model flag spelling、resume flag、履歴探索/forget の store 実装に限られる。既存 #119 はこの重複解消を直接扱っており、#134 は Gemini/Antigravity への MCP 注入に関係する。この issue は #139 の builder / capability と接続しつつ、MCP 注入は別 issue に残す。

## 変更方針

- `NoInlinePromptAgent` 的な共通 builder を導入する。
  - opening prompt は `session_opening_prompt(initial_prompt)`
  - interactive prompt flag: `-i=<prompt>`
  - headless prompt flag: `-p <prompt>`
  - model flag / resume flag / bypass flag は descriptor で差し替え
- resume/forget は別 trait / strategy にする。
  - Gemini: `.project_root` + `chats/*.json`
  - Antigravity: `history.jsonl` の `workspace`
- `AgentFeature` の Support と adapter capability が一致することをテストする。

## 対象ファイル

- `src/infrastructure/agent/gemini.rs`
- `src/infrastructure/agent/antigravity.rs`
- `src/infrastructure/agent/mod.rs`
- `src/infrastructure/agent/util.rs`
- `src/domain/agent_feature.rs`

## 受け入れ条件

- Gemini / Antigravity の既存 command string test が通る。
- command builder の共通部分が 1 実装になる。
- resume/forget store のテストは CLI ごとの strategy に残る。
- #119 と矛盾せず、必要なら #119 の実装作業として取り込める粒度になっている。

## テスト方針

- `cargo test infrastructure::agent::gemini`
- `cargo test infrastructure::agent::antigravity`
- `cargo test domain::agent_feature`
- prompt quote / dash-leading prompt / missing history の edge case を維持する。

## 非目標

- Gemini / Antigravity に MCP を注入しない（#134 側）。
- `agy` / `gemini` の実 CLI を起動しない。
- support matrix の意味を変更しない。
