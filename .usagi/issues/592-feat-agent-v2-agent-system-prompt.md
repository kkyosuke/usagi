---
number: 592
title: feat(agent): v2 の Agent 起動へ system prompt 注入を追加する
status: done
priority: high
labels: [agent, daemon, core, epic]
dependson: []
related: [139, 142, 254, 530, 531, 537, 32]
created_at: 2026-07-31T00:11:36.506934+00:00
updated_at: 2026-07-31T02:45:35.720848+00:00
---

## 目的

v1 と同様に、v2 で Agent（Claude / Codex / sakana.ai）起動時に「root コーディネータ向け」「session worktree 向け」の system prompt と、local LLM MCP 有効時の delegation instruction を注入できるようにする。この親 issue は設計を記録し、実装は子 issue に分割する。

## 背景

`v1/src/infrastructure/agent/mod.rs` は `ROOT_PROMPT` / `SESSION_WORKTREE_PROMPT` / `LOCAL_LLM_PROMPT` を単一の `session_system_prompt(is_root, is_gemini_agy, local_llm_model)` から合成し、Claude は `--append-system-prompt`、Codex は `-c developer_instructions="…"`（TOML basic string escape）で注入している。

v2 では次を実地に確認した。

- `crates/core/src/domain/agent/mod.rs` の `AgentCapability` に system prompt に相当する capability が存在しない。
- `crates/daemon/src/usecase/claude.rs::render_plan` / `crates/daemon/src/usecase/codex/mod.rs::render_plan` はどちらも system prompt を一切組み立てていない。
- `src/runtime/daemon.rs` の `RootClaudeProvisioner` / `RootCodexProvisioner`（実運用の provisioner）も MCP wiring（`claude_mcp_arguments` / `codex_integration_arguments`）、`--settings` hook JSON（`scoped_settings_json`）、OS sandbox（`claude_sandbox_launcher`）は配線するが、system prompt に相当するものは無い。
- local LLM MCP（v1 の `usagi-llm` サーバ、issue #32 相当）自体が v2 にまだ存在しない（`local_llm` / `usagi-llm` の grep が v1 側にしかヒットしない）。

つまり v2 の Agent は現状、自分が usagi の管理する worktree/root 内で起動していることを一切知らされない。v1 では前提だった「worktree は作らない」「親ディレクトリを触らない」「root はコーディネータに徹する」という指示が session/root いずれにも届いていない、実質的な機能後退である。

## 設計要件（このリポジトリの既存契約に基づく）

- **SSoT**: prompt 本文（root 用 / session worktree 用 / local LLM delegation 用）は `usagi-core` に 1 か所だけ持つ。v1 の文言をそのまま移植し、operator 向けの見え方を変えない。
- **層分離**: core は provider-neutral な「この launch に system prompt を載せるか」という capability/policy だけを持つ。Claude の `--append-system-prompt` argv 化、Codex/sakana.ai の `-c developer_instructions=<TOML basic string>` argv 化は各 adapter 内の renderer に閉じる（[2. アーキテクチャ#agent-launch-boundary](../../document/02-architecture.md#agent-launch-boundary) の既存の役割分担を継承）。
- **client からの raw 入力を信頼しない**: prompt 本文・argv・cwd を client から任意に渡せる設計にしない。日本語文面は `LaunchRequest.scope.session_id.is_some()`（session か root か。既存の `sandbox_mode` と同じ判定源）と、daemon 側の trusted 設定（local LLM 有効フラグ）だけから daemon が一意に選ぶ。`LaunchRequest.initial_prompt`（ユーザーが積んだ opening prompt）とは完全に別物として扱う。
- **durable snapshot vs ephemeral provision**: 生成した prompt 文字列自体は **durable `DurableLaunchSnapshot`/`LaunchPlan.argv` に保存しない**。Claude の `--mcp-config` / `--settings`、Codex の `-c mcp_servers...` / `-c hooks.SessionStart...` が既にすべて ephemeral `SpawnProvision.arguments()` 側で毎 `resolve()` 呼び出しごとに再構築されているのと同じ扱いにする。理由:
  - `AgentAdapter::resolve()` は新規起動・明示 resume・（interrupted runtime の）replacement spawn のいずれでも必ず一度だけ、その時点の `LaunchRequest.scope` から再実行される（既存コードで確認済み。resume 済み snapshot を「再生」して spawn し直す経路は無い）。
  - daemon 再起動時の reconcile は既存 live record を `ReconcileRequired` にするだけで再 spawn しないため、prompt を durable に持つ理由がない。
  - 既存の adapter `PROFILE_REVISION` fencing がそのまま prompt 文言の変更にも適用される（文言を変えたら revision を上げる。snapshot に text を持たないため追加の migration 分岐は不要）。
  - 生成された prompt テキスト自体は host path や credential を含まない非 secret 値だが、Claude/Codex とも既存の MCP/hook 系 argv は全て ephemeral 側にあるため、system prompt だけ durable 側に置くと「adapter が注入する CLI flag の保存先」が 2 系統に割れてしまう。1 系統に統一する。
- **capability と fail-closed**: `AgentCapability::SystemPrompt` を追加し、Claude/Codex/sakana.ai の `AgentProfile` へ宣言する。McpWiring と同様に、実運用の `LaunchRequest` 構築箇所（`crates/daemon/src/usecase/agent_ipc.rs` 等）は毎回 `required_capabilities` に `SystemPrompt` を含める。これにより、将来 provider を追加した際に system prompt 配線を忘れる／capability を宣言し忘れると `validate_request` が `LaunchValidationError::UnsupportedCapability` で fail-closed になり、instruction 抜けの状態で launch が黙って成立することを防ぐ。
  - v1 にある Gemini/Antigravity 向けの「system-prompt flag が無い CLI では opening prompt の先頭に前置する」フォールバック（`session_opening_prompt`）は、v2 に該当 adapter が存在しないため本 epic のスコープ外とする。将来 Gemini/Antigravity を v2 に移植する issue で、同じ capability 契約に「initial prompt lead」代替経路を足す設計判断が必要になる旨を申し送る。
- **local LLM delegation**: `LOCAL_LLM_PROMPT` 相当の delegation instruction を合成できるよう、core の prompt builder は bool 引数（例: `local_llm_delegation`）を受け取れるようにする。実際に true を渡す trigger（v2 の local LLM MCP 配線）はまだ存在しないため、別 issue で扱う。

## 子 issue

| # | 内容 | dependson |
|---|---|---|
| core SSoT + capability | prompt 本文・capability・fail-closed 検証を `usagi-core` に追加 | なし |
| Claude/Codex/sakana.ai 配線 | `--append-system-prompt` / `-c developer_instructions=` を実際の provisioner に配線 | core issue |
| local LLM MCP 配線 | v2 に `usagi-llm` MCP を追加し delegation flag を有効化 | core issue, 配線 issue |

（実際の issue 番号は作成後にこの本文へ追記せず、`related`/`dependson` フィールドで表現する。）

## 受け入れ条件

- 3 子 issue が実装しやすい粒度に分割され、依存関係が `dependson` で表現されている。
- 既存の #139/#142/#254/#530/#531/#537（agent capability 基盤・Claude/Codex builder・phase hook/MCP 配線・guard-workspace・sandbox）との重複がないことが本文に明記されている。
- v1 の #32（local LLM MCP）との関係が明記されている。

## 非目標

- この親 issue 自体は実装を行わない。
- Gemini/Antigravity の v2 移植、および同 CLI 向けの system-prompt-less フォールバック経路の実装。
- v1 の shell 文字列前提の escaping ロジックをそのまま移植すること（v2 は argv ベースの `LaunchPlan`/`SpawnProvision` のため shell escaping は原則不要。TOML basic string escaping のみ Codex 側で必要）。
