---
number: 146
title: refactor(orchestration): session agent override 検証を Agent capability に接続する
status: todo
priority: medium
labels: [refactor, orchestration, agent, mcp, review]
dependson: [139]
related: [99, 134, 135]
parent: 137
created_at: 2026-07-06T00:22:01.438015+00:00
updated_at: 2026-07-06T00:22:01.438015+00:00
---

## 目的

`session_create` / `session_delegate_issue` の agent_cli / model override と、実際に起動できる agent capability を接続し、MCP / TUI / usecase で同じ検証語彙を使えるようにする。

## 背景

`presentation/mcp/session.rs` と `presentation/mcp/usagi.rs` は `resolve_session_agent` で `agent_cli` / `model` を解析し、`SessionAgent` に保存する。一方、インストール済み CLI の列挙は `usecase/agent.rs`、起動時の installed gate は Focus handler、MCP/tool schema の enum は presentation にある。agent capability / support matrix とはまだ接続していないため、Gemini/Antigravity の MCP 対応や model 指定の扱いが複数層に散る。

## 変更方針

- #139 の capability descriptor を読み、session agent override の検証・表示に使える helper を usecase に置く。
- MCP schema の accepted agent list と `AgentCli::ALL` / capability を同期する。
- `resolve_session_agent` は presentation 固有の parse に留め、CLI 可用性や feature support の判断は usecase/domain へ寄せる。
- Focus handler の installed-agent gate と session delegate の gate が同じ helper を使える形にする。

## 対象ファイル

- `src/usecase/agent.rs`
- `src/usecase/settings.rs`
- `src/domain/agent.rs`
- `src/domain/agent_feature.rs`
- `src/presentation/mcp/session.rs`
- `src/presentation/mcp/usagi.rs`
- `src/presentation/tui/home/event/handlers.rs`
- `src/presentation/tui/home/state/mod.rs`

## 受け入れ条件

- MCP tool schema と `AgentCli::ALL` のズレがテストで検知される。
- session agent override の parse / validation が presentation の文字列 match に閉じない。
- Focus の installed CLI gate と MCP の agent_cli parse が同じ domain/usecase 語彙に寄る。
- 既存 `session_create` / `session_delegate_issue` のテストが通る。

## テスト方針

- `cargo test presentation::mcp::session`
- `cargo test presentation::mcp::usagi`
- `cargo test usecase::agent`
- unknown agent_cli / model pass-through / all variants schema のテストを維持・追加する。

## 非目標

- session 作成フロー自体を変更しない。
- 未インストール CLI を全面拒否する policy 変更はこの issue では行わない。
- Gemini/Antigravity の MCP 注入実装は行わない。
