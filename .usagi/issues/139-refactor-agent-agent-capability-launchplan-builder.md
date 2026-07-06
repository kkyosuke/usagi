---
number: 139
title: refactor(agent): Agent capability と LaunchPlan builder の土台を追加する
status: done
priority: high
labels: [refactor, agent, review]
dependson: []
related: []
parent: 137
created_at: 2026-07-06T00:19:39.800692+00:00
updated_at: 2026-07-06T04:01:26.093880+00:00
---

## 目的

各 agent adapter の command line 生成を移行する前に、CLI capability と launch/headless plan を表す小さな型を追加する。

## 背景

`domain/agent.rs` の `Agent` trait は `launch_command` / `headless_command` / `has_resumable_session` / `forget_session` を 1 つの port にまとめている。実装側では Claude / Codex / Gemini / Antigravity がそれぞれ model flag、MCP 設定、hook 設定、system prompt、opening prompt、resume、headless bypass を直接文字列として組み立てている。`domain/agent_feature.rs` には support matrix があるが、実際の command builder とは接続していない。

## 変更方針

- 既存 command string は変えず、まず型だけ追加する。
- 例:
  - `AgentCapabilities { mcp, local_llm_mcp, phase_reporting, system_prompt, initial_prompt, resume, forget_history }`
  - `LaunchMode::{Interactive, Headless}`
  - `LaunchRequest { wiring, resume, initial_prompt, prompt, mode }`
  - `LaunchPlan { program, args, shell_escaped }` または現行の shell string へ安全に戻せる builder
- `AgentFeature::support` と capability descriptor が二重 SSoT にならないよう、どちらか一方から導出する道筋を作る。
- 既存 adapter はまだ旧実装を使ってよい。必要ならテスト内だけで descriptor を検証する。

## 対象ファイル

- `src/domain/agent.rs`
- `src/domain/agent_feature.rs`
- `src/infrastructure/agent/mod.rs`
- `src/infrastructure/agent/util.rs`

## 受け入れ条件

- 既存 `Agent` trait の公開挙動は変わらない。
- 新しい capability / launch plan 型に単体テストがある。
- `AgentCli::ALL` の全 variant が capability descriptor で網羅され、追加漏れが compile/test で検知される。
- 後続 issue が adapter を 1 CLI ずつ移行できる。

## テスト方針

- `cargo test domain::agent_feature`
- `cargo test infrastructure::agent`
- 既存 launch command snapshot 的 assertion が壊れていないことを確認する。

## 非目標

- adapter の command string をこの issue で全面移行しない。
- Gemini / Antigravity の MCP 注入対応は行わない。
- Agent CLI の model 名 allowlist や設定 UI は追加しない。
