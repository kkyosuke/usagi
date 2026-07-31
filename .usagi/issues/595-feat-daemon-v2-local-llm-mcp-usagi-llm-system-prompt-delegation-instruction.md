---
number: 595
title: feat(daemon): v2 に local LLM MCP (usagi-llm) 配線を追加し system prompt の delegation instruction を有効化する
status: done
priority: medium
labels: [daemon, agent, mcp]
dependson: [593, 594]
related: [32]
parent: 592
created_at: 2026-07-31T00:13:01.936046+00:00
updated_at: 2026-07-31T02:11:19.317718+00:00
---

## 目的

v1 の #32（ローカル LLM を MCP として公開しクラウド Agent のトークン消費を抑える）相当の機能を v2 に移植し、#593 の `session_system_prompt(is_root, local_llm_delegation)` の `local_llm_delegation` を実際に `true` にする trigger を配線する。

## 背景

v1 は `Settings.local_llm`（`enabled` / `model`）を持ち、有効時に Claude/Codex の MCP server 一覧へ `usagi-llm`（`usagi llm-mcp --model <model>`）を追加し、同時に system prompt へ `LOCAL_LLM_PROMPT`（delegation instruction）を合成する（`v1/src/infrastructure/agent/util.rs` の `USAGI_LLM_MCP_SERVER_NAME` / `v1/src/infrastructure/agent/mod.rs` の `session_system_prompt`）。

v2 には `local_llm` / `usagi-llm` に相当する設定・MCP wiring が一切存在しない（grep で v1 側にしかヒットしない）。現状 v2 は `usagi` MCP サーバのみを Claude (`--mcp-config`) / Codex (`-c mcp_servers.usagi...`) に配線している。

## 変更方針

- v2 の設定層（`crates/core/src/domain/settings` または該当 workspace/global config）に、v1 の `local_llm.enabled` / `local_llm.model` に相当する trusted な設定値を追加する。client からの任意入力ではなく、daemon が読む設定ファイル／config store 由来であることを維持する。
- `RootClaudeProvisioner::provision` / `RootCodexProvisioner::provision`（`src/runtime/daemon.rs`）に、設定が有効な場合だけ `usagi-llm` MCP server 定義（`usagi llm-mcp --model <model>`）を `claude_mcp_arguments` / `codex_integration_arguments` 相当の箇所に追加する。v1 の「`usagi` → `usagi-llm` の順を保つ」（v1 `claude.rs` doc comment 参照）を踏襲する。
- 同じ設定値から `local_llm_delegation: bool` を導出し、#594 で配線した `session_system_prompt(is_root, local_llm_delegation)` 呼び出しへ渡す。
- ローカル LLM 側の MCP server が起動できない・model 名が不正な場合の扱い（v1 の `storage.rs` に見られる allowlist によるサニタイズ相当）を v2 の設定検証に持ち込む。

## 対象ファイル

- `crates/core/src/domain/settings/mod.rs`（または新設する local LLM 設定モジュール）
- `src/runtime/daemon.rs`（`RootClaudeProvisioner` / `RootCodexProvisioner`）
- `crates/daemon/src/usecase/claude.rs` / `crates/daemon/src/usecase/codex/mod.rs`（MCP server 一覧の helper）
- `document/02-architecture.md` / `document/07-mcp.md`（`usagi-llm` MCP wiring の SSoT 追記）

## 受け入れ条件

- local LLM 設定が無効な場合、Claude/Codex の起動 argv・system prompt のいずれにも `usagi-llm` / delegation instruction が一切現れない（現状の非侵襲な挙動を維持する）。
- 設定が有効な場合、`usagi-llm` MCP server が `usagi`（既存の usagi MCP server）に続けて追加され、かつ system prompt に delegation instruction が合成される。
- model 名に shell/TOML/JSON メタ文字を含む不正な設定値を与えても、argv・TOML override・JSON 双方で injection が起きないことをテストで示す（v1 の `storage.rs` テスト `load_settings_sanitizes_a_hand_edited_local_llm_model` 相当の回帰テスト）。

## テスト方針

- `cargo test -p usagi-core domain::settings`
- `cargo test -p usagi-daemon usecase::claude`
- `cargo test -p usagi-daemon usecase::codex`
- `cargo test -p usagi --bin usagi`

## 非目標

- ローカル LLM 側の実際の推論バックエンド（Ollama 等）の追加・変更。
- Gemini/Antigravity への delegation instruction 配線。
