---
number: 142
title: refactor(agent): Claude/Codex launch 生成を共通 builder へ段階移行する
status: done
priority: medium
labels: [refactor, agent, review]
dependson: [139]
related: [120, 122]
parent: 137
created_at: 2026-07-06T00:20:19.286392+00:00
updated_at: 2026-07-06T00:20:19.286392+00:00
---

## 目的

Claude / Codex / codex-fugu の MCP・hook・system prompt・model・headless 生成を `LaunchPlan` builder へ移行し、文字列組み立ての重複と capability matrix との乖離を減らす。

## 背景

Claude と Codex はどちらも usagi MCP、local LLM MCP、phase hook、system prompt、model flag、headless を持つが、それぞれ JSON / TOML override / shell quote の組み立てを adapter 内に抱えている。Codex は `codex` と `codex-fugu` を parameterized adapter にしているが、Claude と Codex の共通概念（MCP server 台帳、phase 語彙、guard-workspace hook、headless では hook を外す等）は別々に表現されている。既存 #120/#122 と関連するが、この issue は #139 の builder 土台に沿って実移行する作業単位とする。

## 変更方針

- #139 の `LaunchPlan` / capability を使う。
- まず Claude と Codex の共通 vocabulary を builder へ移す。
  - MCP server spec: `usagi`, `usagi-llm`
  - phase hook spec: ready/running/waiting/ended
  - guard-workspace hook spec
  - system/developer instruction text
  - model flag rendering
- CLI 固有の出力形式は adapter-specific renderer に残す。
  - Claude: `--mcp-config` JSON / `--settings` JSON / `--append-system-prompt`
  - Codex: `-c` TOML override / hook override / `developer_instructions`
- `launch_command` と `headless_command` の既存文字列を維持する。

## 対象ファイル

- `src/infrastructure/agent/claude.rs`
- `src/infrastructure/agent/codex.rs`
- `src/infrastructure/agent/mod.rs`
- `src/infrastructure/agent/util.rs`
- `src/domain/agent.rs`
- `src/domain/agent_feature.rs`

## 受け入れ条件

- Claude / Codex / codex-fugu の既存 launch/headless command assertion がすべて通る。
- MCP server 名、phase 名、guard-workspace hook が adapter ごとの直書きから共通 spec へ寄る。
- adapter は CLI 固有の syntax rendering と resume/forget に集中する。
- `codex-fugu` は引き続き `codex` と同じ feature set を持つ。

## テスト方針

- `cargo test infrastructure::agent::claude`
- `cargo test infrastructure::agent::codex`
- `cargo test infrastructure::agent`
- single quote / Windows path / prompt starts with dash の既存 edge case を維持する。

## 非目標

- Gemini / Antigravity の統合はこの issue では扱わない。
- agent CLI の実行確認やネットワークアクセスは行わない。
- command string の外部仕様を変更しない。
