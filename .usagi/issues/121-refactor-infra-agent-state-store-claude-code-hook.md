---
number: 121
title: refactor(infra): agent_state_store から Claude Code hook ペイロード解析を分離する
status: todo
priority: medium
labels: [refactor, infra, review]
dependson: []
related: []
created_at: 2026-07-04T23:14:56.364288+00:00
updated_at: 2026-07-04T23:14:56.364288+00:00
---

## 背景（なぜ問題か）

`infrastructure/agent_state_store.rs` は本来 worktree 別の agent phase を永続化するストアだが、`worktree_from_hook_json` / `tool_path_from_hook_json` / `session_start_source_from_hook_json` という **Claude Code の hook ペイロードを serde_json でパースするだけの関数**を抱えている。これらは `dir`/`key`/`json_file` を一切使わず、phase 永続化とは別関心であり、責務が混在している。

（#59（done）は agent_state の「遷移ポリシー」の usecase 移設が対象で、この hook 解析関数群は別スコープ。）

## 対象箇所

- `src/infrastructure/agent_state_store.rs` の hook パーサ 3 関数（`worktree_from_hook_json` / `tool_path_from_hook_json` / `session_start_source_from_hook_json`）

## やること

- `infrastructure/agent/hook_payload.rs`（仮）等へ移設し、`agent_state_store` は phase 永続化に専念させる。

## 受け入れ条件

- 呼び出し元（agent-phase CLI 等）の import 差し替えのみで挙動不変。テストも移設先へ移動。
- 既存テストが緑、カバレッジ 100% 維持。
