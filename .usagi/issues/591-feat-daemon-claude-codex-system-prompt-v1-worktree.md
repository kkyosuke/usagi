---
number: 591
title: feat(daemon): Claude/Codex 起動に system prompt 注入を追加する(v1 の worktree 境界指示を移植)
status: todo
priority: high
labels: [daemon, feat, agent]
dependson: []
related: []
created_at: 2026-07-30T23:26:23.643028+00:00
updated_at: 2026-07-30T23:26:23.643028+00:00
---

## 背景

v1 は agent 起動時に、system prompt を持つ CLI には out-of-band で、持たない CLI には開始プロンプトの先頭に前置する形で、共通の指示文を注入していた（`v1/src/infrastructure/agent/mod.rs:26-104`）。

- `ROOT_PROMPT`: workspace root（コーディネータ）向けの役割説明。
- `SESSION_WORKTREE_PROMPT`: 「すでに session 専用の worktree にいるので新たに worktree を作る必要はない」「作業はこのディレクトリ配下だけで完結させる」「親ディレクトリを読み書き・cd しない」という指示。**実際に現在この v1 usagi によって起動されている本セッションの system prompt もこの文言そのものである**(このセッションの `<context>`/`<constraints>`/`<instructions>` ブロックと完全一致)。
- Claude は `--append-system-prompt`(`v1/src/infrastructure/agent/claude.rs:189,222`)、Codex は `-c developer_instructions=...`(`v1/src/infrastructure/agent/codex.rs:205-208`)で out-of-band 注入し、system prompt flag を持たない Gemini/Antigravity は開始プロンプトの先頭に前置する(`session_opening_prompt`)。

**v2 にはこの仕組みが存在しない。**

- `crates/daemon/src/usecase/claude.rs` と `crates/daemon/src/usecase/codex/mod.rs` はいずれも `request.initial_prompt`(ユーザーの依頼内容)しか argv に載せておらず、`--append-system-prompt` や `developer_instructions` の構築コードが無い(grep 0件)。
- `document/` 配下に "system prompt" という語が一度も出てこない。
- v2 の worktree 境界は `document/02-architecture.md`「Claude 起動の多層防御」節にある通り、**書き込みを強制的に制限する2層**(論理境界の `PreToolUse` フック `guard-workspace`、hard boundary の OS sandbox `claude-sandbox`)だけで守っており、エージェントに「なぜ・どう振る舞うべきか」を伝えるテキストレベルの手段を持たない。書き込み制限はできても、**エージェントの行動方針(worktree 前提の作業指示、コーディネータとしての振る舞い等)を事前に伝える手段が無い**。

## 設計方針

v1 の構造をそのまま v2 のクリーンアーキテクチャに落とし込む。既存の類似実装(`crates/daemon/src/usecase/claude.rs` の `scoped_settings_json` と、composition root `src/runtime/daemon.rs` の `claude_settings_arguments` / `codex_integration_arguments` の役割分担)と同じパターンに揃える。

1. **新規モジュール `crates/daemon/src/usecase/agent_prompt.rs`**(Claude/Codex 両アダプタが使う共有ロジックなので、両者から見える `usagi-daemon::usecase` に置く。`usagi-core` へは置かない — これは daemon 面専用の launch 構築ロジックであり、他クレートは参照しない)。
   - `LaunchScope`(`usagi_core::domain::agent::LaunchScope`)から root か session かを判定し、対応する定数テキストを返す純粋関数 `pub fn session_system_prompt(scope: &LaunchScope) -> String` を置く。
   - テキストは v1 の `ROOT_PROMPT` / `SESSION_WORKTREE_PROMPT` を移植する(文言は v1 と同一でよい。本セッションの `<context>`/`<constraints>`/`<instructions>` ブロックが実例)。
   - secret・host-specific path を含まない純粋な文字列生成なので、`usecase` 層に置いて問題ない(`scoped_settings_json` も同様に usecase に置かれている)。

2. **Claude 側の配線**: composition root `src/runtime/daemon.rs` に `claude_system_prompt_arguments(scope: &LaunchScope) -> Vec<String>`(`vec!["--append-system-prompt".into(), agent_prompt::session_system_prompt(scope)]`)を追加し、`RootClaudeProvisioner::provision`(`src/runtime/daemon.rs:443-489` 付近)の `arguments` へ `claude_settings_arguments` と同様に extend する。

3. **Codex 側の配線**: composition root に `codex_system_prompt_arguments(scope: &LaunchScope) -> Result<Vec<String>, ()>` を追加し、既存 `codex_integration_arguments`(`src/runtime/daemon.rs:658-686`)と同じ `-c key = value` 形式(TOML basic string として `serde_json::to_string` で escape する既存パターンを使う。v1 の `toml_basic_string` 相当)で `-c developer_instructions = "..."` を返す。`RootCodexProvisioner::provision`(`src/runtime/daemon.rs:401-427`)の `arguments` へ、既存の `context.inject_mcp.then(...)` extend と並べて **MCP wiring の有無に関わらず常に** 追加する(system prompt 注入は MCP wiring とは独立)。同じ配線は Codex 互換の `sakana-ai` profile(`CodexAdapter::fugu`)にも及ぶ(同じ `CodexProvisioner` を共有するため追加変更は不要)。

4. **argv の扱い**: `render_plan`(`claude.rs` / `codex/mod.rs`)は変更しない。system prompt の追加引数は `SpawnProvision.arguments`(非 durable)経由で渡す(既存の `--settings` / MCP config と同じ経路)。テキスト自体は secret を含まないため durable snapshot に含めても実害はないが、既存の hook/MCP 配線と対称にするため同じ非 durable 経路に揃える。

## 対象外(将来拡張)

- v1 の `LOCAL_LLM_PROMPT`(ローカル LLM MCP への委譲を促す nudge)に相当する概念は v2 にまだ存在しない(grep 0件)。本 issue のスコープ外とする。
- v1 の `session_opening_prompt`(system prompt flag を持たない CLI 向けに開始プロンプト先頭へ前置する fallback)に相当する仕組みも本 issue では実装しない。v2 は現時点で `claude` / `codex` / `sakana-ai` の3 profile しか登録しておらず(`crates/daemon/src/usecase/orchestration.rs`)、すべて out-of-band 注入に対応するため fallback 経路は必要ない。Gemini/Antigravity 相当の profile を v2 に追加する際に再検討する。
- `AgentCapability` への新規 variant 追加(例: `SystemPrompt`)は、現在登録済みの全 profile が対応するため今は不要と判断し追加しない(すべての profile が対応する capability をわざわざ closed vocabulary に加える過剰な抽象化を避ける)。将来 system prompt 未対応の profile を追加する際に capability として導入する。

## 受入条件

- [ ] `crates/daemon/src/usecase/agent_prompt.rs`(新規)に `session_system_prompt(scope: &LaunchScope) -> String` があり、root/session で異なるテキストを返すユニットテストがある。
- [ ] Claude 起動の `SpawnProvision.arguments` に `--append-system-prompt <text>` が含まれ、root 起動と session 起動でテキストが異なることを検証するテストがある(既存の `claude_settings_arguments` のテストと同じ粒度)。
- [ ] Codex(および `sakana-ai`)起動の `SpawnProvision.arguments` に `-c developer_instructions = "..."` が含まれ、TOML として正しく escape されていることを検証するテストがある。
- [ ] 既存の MCP wiring・hook 配線・sandbox 起動に regression がない。
- [ ] `document/05-daemon.md` または `document/02-architecture.md` の Claude 起動節に、system prompt 注入の契約(何を・いつ・どちらの経路で)を追記する。
- [ ] `cargo test -p usagi-daemon --bin usagi` および root の統合テストが green。
