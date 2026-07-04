---
number: 47
title: refactor(agent): Agent アダプタの二重間接と program/command の重複を解消する
status: done
priority: low
labels: [refactor, agent]
dependson: []
related: []
created_at: 2026-06-18T22:41:24.980332+00:00
updated_at: 2026-07-04T00:15:26.138721+00:00
---

## 背景

Agent CLI（claude / gemini）の起動コマンド生成ロジックは `domain/settings.rs` の `AgentCli::launch_command`（大きな `match self`）に集約されているのに、その上にもう一段 `domain::agent::Agent` trait と `ClaudeAgent`/`GeminiAgent` アダプタがあり、両アダプタの `launch_command` は `AgentCli::Claude.launch_command(...)` などへ素通しするだけになっている。

その結果「CLI ごとの分岐」が、`AgentCli::launch_command` の `match`・`infrastructure/agent/mod.rs` の `agent_for` の `match`・`AgentCli::command` の `match` と複数箇所に分散している。さらに各アダプタの `program()` は `AgentCli::command()` と同じ文字列（`"claude"`/`"gemini"`）を別実装で重複定義している。アダプタ固有のロジックは実質 Claude の `has_resumable_session`（トランスクリプト探索）だけ。

## 改善方針

次のいずれかで「分岐 1 箇所・命名 1 定義」に寄せる。

- (A) アダプタ層を残すなら、launch_command 生成と program 名の本体をアダプタ側へ移し、`AgentCli::launch_command` / `AgentCli::command` を domain から削除する（domain には `AgentWiring` の policy だけ残す）。分岐がアダプタ 1 箇所に集約される。
- (B) アダプタが `has_resumable_session` 以外でほぼ無価値なら、`Agent` trait を `has_resumable_session` 中心に縮め、launch は `AgentCli` のメソッドのまま使う。

いずれの案でも `program()` と `command()` の二重定義は解消する。

## 確認方法

- `claude` / `gemini` の起動コマンド（MCP・フック注入を含む）が従来どおり生成されること（settings / agent テスト）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
