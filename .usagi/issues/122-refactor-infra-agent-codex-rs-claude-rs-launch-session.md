---
number: 122
title: refactor(infra): agent/codex.rs・claude.rs を「launch 生成」と「session 探索」に分割する
status: todo
priority: medium
labels: [refactor, agent, review]
dependson: []
related: [119, 120]
created_at: 2026-07-04T23:15:08.897718+00:00
updated_at: 2026-07-04T23:15:08.897718+00:00
---

## 背景（なぜ問題か）

実コード行数で `codex.rs` ≈ 400 行・`claude.rs` ≈ 325 行と、規約の「1 ファイル 300 行超は分割検討」を超える。両者とも ① launch コマンド生成（エンコード用 struct/ヘルパ群）と ② resume/forget 用のセッション探索（`*_projects_root` / `*_sessions_root` / `rollout_cwd` / `collect_rollouts` / `*_in`）という、**状態を共有しない 2 責務**を同居させている。

## 対象箇所

- `src/infrastructure/agent/codex.rs`
- `src/infrastructure/agent/claude.rs`

## やること

- `agent/codex/launch.rs` + `agent/codex/transcripts.rs`（claude も同様に `agent/claude/launch.rs` + `agent/claude/transcripts.rs`）へ組織的に分割し、各ファイルを 300 行未満にする。

## 受け入れ条件

- 公開 API 不変、モジュール分割のみ。各新ファイルが 300 行前後以内。
- 既存テストが緑、カバレッジ 100% 維持。

## 補足

同じ agent アダプタ整理の #119（gemini/antigravity 統合）・#120（SSoT 化）と related。分割は SSoT 化（#120）を入れやすくする土台にもなる。
