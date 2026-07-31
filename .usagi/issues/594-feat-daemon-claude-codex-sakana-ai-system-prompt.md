---
number: 594
title: feat(daemon): Claude/Codex/sakana.ai 起動へ system prompt を配線する
status: todo
priority: high
labels: [daemon, agent, claude, codex]
dependson: [593]
related: [142, 530, 537]
parent: 592
created_at: 2026-07-31T00:12:38.550549+00:00
updated_at: 2026-07-31T00:12:38.550549+00:00
---

## 目的

#593 で追加した core の system prompt SSoT を、実際の Claude / Codex / sakana.ai 起動 argv へ配線する。Claude は `--append-system-prompt`、Codex/sakana.ai は `-c developer_instructions=<TOML basic string>` を使う。

## 背景

実運用の起動組み立ては `src/runtime/daemon.rs` の `RootClaudeProvisioner::provision` / `RootCodexProvisioner::provision` が担う。両者はそれぞれ `claude_mcp_arguments` + `claude_settings_arguments`（Claude）、`codex_integration_arguments`（Codex/sakana.ai。sakana.ai も同じ `RootCodexProvisioner` を `program` フィールドだけ変えて共有している）で MCP/hook 配線の argv を組み、`SpawnProvision::new(...)` の `arguments` に積んでいる。最終的な子プロセス argv は `AgentPty::spawn`（`src/runtime/daemon.rs:1006` 付近）が `provision.arguments()` → `plan.argv` の順に連結して構築する。v2 は shell 文字列ではなく argv ベクタなので、v1 のような shell single-quote escaping は不要（このこと自体が v1 からの簡素化点であり、退行させない）。

## 変更方針

- **Claude** (`RootClaudeProvisioner::provision`): `session_system_prompt(is_root, local_llm_delegation)` を呼び、`["--append-system-prompt".into(), prompt]` を `arguments`（`claude_mcp_arguments`/`claude_settings_arguments` と同じ ephemeral `SpawnProvision.arguments()`）へ追加する。`is_root` は既存の `sandbox_mode(context) == SandboxMode::Root` と同じ判定式を再利用する（新しい判定ロジックを作らない）。`local_llm_delegation` はこの issue では常に `false`（#592 の local LLM MCP 配線 issue で trigger する）。
  - argv は 1 要素として渡すため JSON/shell escaping は不要。ただし prompt 本文にアポストロフィ・二重引用符・バックスラッシュ・改行が混じっても argv ベクタが破綻しないことを回帰テストで示す（v1 の shell string 前提が復活しないためのガード）。
- **Codex/sakana.ai** (`RootCodexProvisioner::provision`): 同じ `session_system_prompt` を呼び、TOML basic string escape（v1 の `toml_basic_string`: `\` → `\\`、`"` → `\"`）した上で `-c developer_instructions="…"` の 1 argv トークンとして `codex_integration_arguments` と同じ ephemeral `arguments` に追加する。既存の `-c mcp_servers.*` / `-c features.hooks` / `-c hooks.SessionStart` の並びに影響しない位置（`--` より前）に置く。
- **重複注入の防止**: `AgentAdapter::resolve()` は起動ごとに一度しか呼ばれない前提だが、テストで `argv` 中に `--append-system-prompt` / `developer_instructions=` が高々 1 回しか出現しないことを明示的に assert する回帰テストを追加する。
- **resume / replacement spawn**: `resume`（`--resume` / `--session-id` / Codex の `resume <id>`）は `provision.spawn.append_sensitive_arguments(...)` で system prompt 配線の**後に**追加されるため、順序が壊れないことをテストで確認する。resume・interrupted runtime の replacement spawn いずれも `resolve()` を再実行するため、その都度現在の `scope`/設定から system prompt が再生成されることをテストで示す（stale なテキストを再利用しない）。

## 対象ファイル

- `src/runtime/daemon.rs`（`RootClaudeProvisioner::provision` / `RootCodexProvisioner::provision` / 関連 helper・テスト）
- `crates/daemon/src/usecase/claude.rs`（必要なら helper の置き場所調整）
- `crates/daemon/src/usecase/codex/mod.rs` / `crates/daemon/src/usecase/codex/fixture.rs`
- `document/02-architecture.md`（「Claude 起動の多層防御」表に `--append-system-prompt` の配線を追記。Codex 側の `-c developer_instructions` も同節または隣接箇所に追記）

## 受け入れ条件

- Claude の起動 argv に `--append-system-prompt <session_system_prompt(...)>` が過不足なく 1 回だけ含まれる（新規 interactive / headless / resume interactive / replacement spawn のいずれでも）。
- Codex・sakana.ai の起動 argv に `-c developer_instructions="<TOML escaped>"` が 1 回だけ含まれ、TOML として正しくパースできる文字列であることをテストで示す（バックスラッシュ・二重引用符・制御文字を含む入力での escape テスト）。
- root 起動時は `ROOT_PROMPT` 相当、session 起動時は `SESSION_WORKTREE_PROMPT` 相当のテキストが載ることを確認する（`sandbox_mode` の判定と一致）。
- system prompt の内容が `DurableLaunchSnapshot`（`LaunchPlan.argv` や `plan` の JSON シリアライズ結果）に一切現れないことをテストで示す（#592 の durable/ephemeral 方針の担保）。
- sandbox launcher（`claude_sandbox_launcher` の prefix）で argv を包んでも `--append-system-prompt` の位置・内容が壊れないことを確認する。
- Codex の `-c` override の並び（mcp_servers → hooks → developer_instructions、または既存順を崩さない）と `--` 以降の prompt 位置関係が既存テストと矛盾しないことを確認する。

## テスト方針

- `cargo test -p usagi --bin usagi`（`src/runtime/daemon.rs` のユニットテスト。既存の `scoped_settings_json` / guard-workspace テスト群に隣接して追加）
- `cargo test -p usagi-daemon usecase::claude`
- `cargo test -p usagi-daemon usecase::codex`
- 既存の `crates/daemon/tests/agent_real_pty.rs` 等の実 PTY テストがあれば、system prompt 込みの argv で起動が壊れないことを確認する（新規実行はしない。既存 fixture の拡張のみ）。

## 非目標

- local LLM delegation instruction を実際に有効化する trigger（#592 の local LLM MCP 配線 issue）。
- Gemini/Antigravity 向けの配線。
- v1 の shell string 前提の `shell_single_quote` をそのまま移植すること（argv ベースのため不要）。
