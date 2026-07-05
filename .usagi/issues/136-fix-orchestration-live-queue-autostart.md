---
number: 136
title: fix(orchestration): live queue に滞留したプロンプトを autostart が救済せず自動起動が不発になる
status: done
priority: high
labels: [orchestration, tui]
dependson: []
related: [98]
created_at: 2026-07-05T00:44:18.090885+00:00
updated_at: 2026-07-05T00:50:41.225151+00:00
---

## 背景（症状）

`session_prompt` / `session_delegate_issue` で委譲しても、対象セッションの agent ペインが自動起動しないことがある（#98 で入れた autostart が不発）。

## 根本原因（実測で確定）

- `session_prompt` の `auto` モードは `agent_is_live` でライブ判定して live queue / launch queue を振り分ける（`src/presentation/mcp/session.rs:235`）。
- 本番の `agent_is_live` は「agent-phase ファイルが存在するか」だけで判定する（`src/main.rs:54`、`agent_state_store::read().is_some()`）。
- ところが phase ファイルは agent 終了後も `ended` のまま残る（`✓ done` バッジ表示のため／TUI 異常終了で `clear` が走らない）。→ ペインが無いのに `auto` が live 判定 → **live queue（`~/.usagi/agent-live-prompts/`）へ**。
- live queue は「既存の稼働ペインに貼り付ける」だけで**ペインを spawn しない**。autostart は **launch queue（`agent-prompts/`）しか見ない**（`src/presentation/tui/home/mod.rs:1765`）。→ プロンプトが live queue に滞留したまま**永久に起動されない**。

## 方針（`agent_is_live` は変えない）

`agent_is_live` を `is_active()` に変えると「ターン完了で `ended` になった"開いたまま idle のペイン"への live 配信」を壊すため不可。代わりに、実際のペイン生存を権威的に知る TUI 側（`pool.has_live_pane` = PTY aliveness）で救済する。

- TUI の autostart パス（`autostart_queued_prompts`）を、ペイン非在の worktree について **launch queue と live queue の両方**からプロンプトを取り出し、まとめて fresh agent の opening message として spawn するよう拡張する。
- `agent_live_prompt_store::any_queued()` を追加（launch 側と対称、cheap gate 用）。
- これで `ended`／stale `running`／TUI kill の全ケースを、開いた idle ペインの live 配信を壊さずに解消する。設定 OFF 時は従来どおり待機（後方互換）。

## 受け入れ条件

- live queue に積まれたプロンプトが、ペインの無いセッションで TUI 稼働中に自動 spawn される。
- 既存の「開いたペインへの live 配信」「launch queue の autostart」は不変。
- `agent_live_prompt_store::any_queued()` にユニットテスト。ドキュメント（03-mcp / 04-orchestration）を更新。
