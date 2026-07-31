---
number: 593
title: feat(core): agent launch に system prompt SSoT と capability を追加する
status: todo
priority: high
labels: [core, agent, review]
dependson: []
related: [139]
parent: 592
created_at: 2026-07-31T00:12:05.285547+00:00
updated_at: 2026-07-31T00:30:59.283082+00:00
---

## 目的

#592 の設計に基づき、system prompt 本文の単一情報源（SSoT）と `AgentCapability::SystemPrompt` を `usagi-core` に追加する。この issue では実際の CLI 配線（`--append-system-prompt` / `developer_instructions`）は行わない。

## 背景

`v1/src/infrastructure/agent/mod.rs` の `ROOT_PROMPT` / `SESSION_WORKTREE_PROMPT` / `LOCAL_LLM_PROMPT` / `session_system_prompt` が正本の文言・合成ロジックである。v2 の `crates/core/src/domain/agent/mod.rs` にはこれに相当するものが無く、`AgentCapability` にも system prompt を表す variant が無い。`crates/daemon/src/usecase/{claude,codex}/*` の `render_plan` も何も注入していない（#592 で確認済み）。

`#139` が `AgentCapability` / `LaunchPlan` の土台を作った際の設計（capability は closed vocabulary、adapter は既存実装のまま追加型だけ先行させる）を踏襲する。

**「main とそれ以外」の対応関係**: この issue で言う `is_root` の二値は、[.agents/workflow.md](../../.agents/workflow.md) が定義する「リポジトリのルート（**main** のチェックアウト）で直接作業している場合」と「usagi セッション worktree（`usagi/<name>` ブランチ）内で起動している場合」の区別そのものである。root = main チェックアウトで動くコーディネータ = `ROOT_PROMPT`、session = `usagi/<name>` の worktree = `SESSION_WORKTREE_PROMPT`。追加の判定軸（例えば root が実際にどの git ブランチをチェックアウトしているか）は設けない。`LaunchScope.session_id.is_none()` だけで一意に決まる。

## 変更方針

- `usagi-core`(例: `crates/core/src/domain/agent/prompt.rs` のような新モジュール、または既存 `domain/agent/mod.rs` 内)に次を追加する。
  - `root_prompt() -> &'static str` / `session_worktree_prompt() -> &'static str` / `local_llm_delegation_prompt() -> &'static str`: v1 の `ROOT_PROMPT` / `SESSION_WORKTREE_PROMPT` / `LOCAL_LLM_PROMPT` と**バイト完全一致**のテキストを移植する（operator に見える指示文を変えない）。
  - `session_system_prompt(is_root: bool, local_llm_delegation: bool) -> String`: v1 の `session_system_prompt(is_root, is_gemini_agy, local_llm_model)` から `is_gemini_agy` 分岐を除いた版（v2 に Gemini/Antigravity adapter が無いため）。`is_root` は呼び出し側が `LaunchRequest.scope.session_id.is_none()` から渡す（`src/runtime/daemon.rs` の既存 `sandbox_mode` と同じ判定式を再利用させる設計にし、判定ロジックの二重化を避ける）。
- `AgentCapability` に `SystemPrompt` variant を追加する（`#[serde(rename_all = "snake_case")]` により `"system_prompt"`）。
- Claude (`crates/daemon/src/usecase/claude.rs`)・Codex/sakana.ai (`crates/daemon/src/usecase/codex/mod.rs`) の `AgentProfile::new(...)` 呼び出しの capability リストへ `AgentCapability::SystemPrompt` を追加する（実際の argv 配線はこの issue では行わない。capability だけを宣言する）。
- 実運用の `LaunchRequest` 構築箇所（`crates/daemon/src/usecase/agent_ipc.rs` の `required_capabilities: [AgentCapability::McpWiring].into_iter().collect()` となっている生成箇所）に `AgentCapability::SystemPrompt` を追加し、`McpWiring` と同格の必須 capability にする。これにより `validate_request` が、system prompt capability を宣言し忘れた profile への launch を `LaunchValidationError::UnsupportedCapability` で fail-closed にする（#592 の fail-closed 設計を担保する変更）。
- `session_system_prompt` は `local_llm_delegation: bool` を受け取れるようにするが、実運用で true を渡す trigger（local LLM MCP 設定）はまだ存在しない。この issue では常に `false` を渡してよい（trigger 配線は別 issue）。

## 対象ファイル

- `crates/core/src/domain/agent/mod.rs`（または新設する `crates/core/src/domain/agent/prompt.rs`）
- `crates/daemon/src/usecase/claude.rs`
- `crates/daemon/src/usecase/codex/mod.rs`
- `crates/daemon/src/usecase/agent_ipc.rs`
- `document/02-architecture.md`（Agent launch boundary 節に capability 追加を反映）

## 受け入れ条件

- `session_system_prompt(is_root, local_llm_delegation)` が v1 の `ROOT_PROMPT` / `SESSION_WORKTREE_PROMPT` / `LOCAL_LLM_PROMPT` とバイト完全一致のテキストを返す（root/session × delegation あり/なしの 4 パターン）。
- `AgentCapability::SystemPrompt` が Claude/Codex/sakana.ai の `AgentProfile` に宣言されている。
- 実運用の `LaunchRequest` 生成箇所すべてで `required_capabilities` に `SystemPrompt` が含まれる。
- `SystemPrompt` capability を宣言しない profile に対して `required_capabilities` が `SystemPrompt` を含む `LaunchRequest` を渡すと、`validate_request` が `UnsupportedCapability` を返すことをテストで示す（fail-closed の回帰テスト）。
- 既存の `Agent`/`LaunchRequest`/`DurableLaunchSnapshot` の JSON round-trip・snapshot schema 互換性が壊れない（新 capability variant の追加は additive）。
- `document/02-architecture.md` の Agent launch boundary 節に、system prompt が何によって選ばれるか（`LaunchScope.session_id` の有無＝main チェックアウトの root か `usagi/<name>` session worktree か・trusted local LLM 設定）と、それが durable snapshot に**保存されない**設計（#592 参照）が追記されている。

## テスト方針

- `cargo test -p usagi-core domain::agent`
- `cargo test -p usagi-daemon usecase::claude`
- `cargo test -p usagi-daemon usecase::codex`
- `cargo test -p usagi-daemon usecase::agent_ipc`

## 非目標

- Claude の `--append-system-prompt` / Codex の `-c developer_instructions=` への実配線（#592 の「Claude/Codex/sakana.ai 配線」issue）。
- local LLM MCP（`usagi-llm`）の実装・設定・delegation flag の実 trigger（#592 の「local LLM MCP 配線」issue）。
- Gemini/Antigravity 向けの opening-prompt-lead フォールバック設計。
- root/session 以外の追加判定軸（実際のチェックアウトブランチ名によるオーバーライド等）を設けること。
