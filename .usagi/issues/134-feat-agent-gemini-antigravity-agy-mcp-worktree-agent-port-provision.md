---
number: 134
title: feat(agent): Gemini / Antigravity(agy) に MCP を注入する（worktree ローカル設定書き出し + Agent port の provision）
status: todo
priority: high
labels: [orchestration, agent]
dependson: [120]
related: [119, 99]
created_at: 2026-07-05T00:29:20.039166+00:00
updated_at: 2026-07-05T00:29:20.039166+00:00
---

## 背景

MCP 注入は現状 Claude（`claude.rs::mcp_config_json`、`--mcp-config` JSON）と Codex（`codex.rs`、`-c` TOML 上書き）のみ。gemini（`gemini.rs`）/ agy（`antigravity.rs`）は設計コメントどおり MCP / フック / system-prompt を **inline 注入しない**ため、これらのセッションは usagi MCP（issue / memory / session）を使えない。

「MCP 対応可否」の SSoT は `src/domain/agent_feature.rs::support(cli, feature)`。現状 `support(Gemini|Antigravity, Mcp) == No`。本 issue はこれを `Yes` に反転させる＝gemini/agy に実際に MCP を配線する。

## 設計方針（`docs-draft/plan-agent-mcp-select.md` §2.1/§2.2 参照）

- **書き込み先**: ユーザー設定（`~/.gemini/...`）ではなく **セッションの worktree 内**（gemini: `<worktree>/.gemini/settings.json` の `mcpServers`、agy: `mcp_config.json` 相当）。usagi が既に `<worktree>/.claude/skills/*` を worktree 内に symlink し git-exclude している前例（`session::create` の skills 配線）に倣う。理由: (1) セッション破棄（`session remove` / `usagi clean`）でツリーごと消えるので**後始末が自動**、(2) ユーザー設定・リポジトリ本体を汚さない、(3) 方針変更を「worktree 内（使い捨てツリー）」に限定できる。
- **git を汚さない**: 書き出したファイルを `git::ensure_all_excluded`（skills と同じ）で worktree の exclude に登録する。
- **副作用を port に載せる**: `Agent` トレイトに `provision(&self, wiring: &AgentWiring, dir: &Path) -> std::io::Result<()>`（既定 no-op）を追加。`launch_command` は純粋な文字列生成（`Result` を返さない）契約を保つため、副作用はこの別メソッドに分離する。宣言は domain（`Path`/`io::Result` のみ、外部クレート非依存）、実装は infrastructure。Claude/Codex は no-op、gemini/agy が worktree ローカル設定を書く。冪等（毎起動で上書き可）。
- **呼び出し箇所**: TUI home の 4 起動経路（`open_terminal` / `start_pending_spawn` / `restore_open_panes` / `autostart_queued_prompts`、`presentation/tui/home/mod.rs`）で `launch_command` 直前に `agent.provision(&wiring, dir)` を呼ぶ。Claude/Codex は no-op なので既存挙動不変。
- **注入内容の SSoT**: 注入する MCP サーバ台帳（`usagi`=`<bin> mcp` / `usagi-llm`=`<bin> llm-mcp --model <m>`）は #120 が中立記述（`Vec<(name, cmd, args)>`）化する。**#120 に dependson**し、その中立台帳を gemini/agy の書き出しでも描画して 4 個目のエンコーダ重複を作らない。
- **matrix 更新**: `agent_feature::support` の `Gemini`/`Antigravity` × `Mcp` / `LocalLlmMcp` を `Yes` に反転。`PhaseReporting`（フック機構なし）と `SystemPrompt`（引き続き開始プロンプト前置で代替）は **No のまま**（本 issue は MCP のみ）。

## 調査ステップ（実装時に確定）

- gemini の MCP 設定の正確なパス・キー構造を `gemini --help` と実挙動で確定（プロジェクトスコープ `<worktree>/.gemini/settings.json` が効くか）。
- agy の MCP 設定ファイル（`mcp_config.json`）のパス・スキーマを確定。`antigravity.rs` は agy が `~/.gemini/antigravity-cli/` を使うことを既に掴んでいる。**プロジェクトスコープ設定が不可能な場合**は、ユーザー設定への worktree キー付き追記＋`forget_session` 相当での掃除を次善策とする（この分岐は要相談）。

## やること

- `Agent` port に `provision` を追加（既定 no-op）。
- gemini / antigravity アダプタに provision 実装（worktree ローカル MCP 設定書き出し + git-exclude）。#120 の中立台帳を利用。
- TUI 4 起動経路で `provision` を呼ぶ配線。
- `agent_feature::support` の gemini/agy × Mcp/LocalLlmMcp を Yes に反転し、行列テストを更新。
- ドキュメント更新（`document/02-architecture.md` の agent アダプタ節、`document/03-commands/03-mcp.md` の「どの CLI に MCP が wire されるか」、必要なら `document/05-settings.md`）。

## 受け入れ条件

- gemini / agy セッションで usagi MCP（issue / memory / session tool）が使える。
- 書き出した設定は worktree 内にあり git を汚さない（session が dirty にならない）。`session remove` でツリーごと消える。
- `usagi feature` の行列で gemini/agy の MCP 行が ✓ になる。
- Claude / Codex の起動コマンド・挙動は不変。
- 既存テスト緑、カバレッジ 100% 維持（provision は tempdir を渡してユニットテスト）。
