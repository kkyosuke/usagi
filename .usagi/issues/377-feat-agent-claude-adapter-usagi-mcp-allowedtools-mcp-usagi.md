---
number: 377
title: feat(agent): Claude adapter が注入する usagi MCP のツールを無確認で許可する（--allowedTools mcp__usagi）
status: done
priority: high
labels: [agent, daemon, mcp, orchestration]
dependson: []
related: [254, 253, 134]
created_at: 2026-07-19T22:39:55.806782+00:00
updated_at: 2026-07-19T22:43:22.491683+00:00
---

## 背景 / 問題

v2 の daemon が agent（Codex / Claude）を起動するとき、合成ルート（`src/runtime/daemon.rs`）の product-specific provisioner が `usagi mcp` を子 MCP server として spawn 時引数で注入する（正本: [document/05-daemon.md#agent-ownership](../../document/05-daemon.md#agent-ownership)、[document/07-mcp.md](../../document/07-mcp.md)）。

- **Codex**: `codex_mcp_arguments` が `-c mcp_servers.usagi.command=… -c mcp_servers.usagi.args=["mcp"]` を注入する。approval policy は `.codex/config.toml` の `approval_policy = "never"` と、adapter が渡す argv（interactive の `--ask-for-approval never` / headless の `--dangerously-bypass-approvals-and-sandbox`）で既に無確認。Codex は `approval_policy = never` のとき MCP tool 呼び出しの確認プロンプトを出さない（`untrusted` / `on-request` のときだけ elicitation で確認する）ため、**Codex は追加対応不要**。
- **Claude**: `claude_mcp_arguments` が `--mcp-config '{"mcpServers":{"usagi":{…}}}'` で server を注入するが、その tool の permission を事前許可していない。Claude Code は既定で tool 呼び出しごとに permission プロンプトを出すため、**agent が usagi MCP tool を呼ぶたびに確認が必要**になり、自律実行（orchestration の委譲・観測・完了報告など）が止まる。

## 目的

v2 の実行経路で、agent が **usagi が注入する MCP tool** を使うときに確認を不要にする。対象は usagi が提供・注入する MCP tool の承認だけに限定し、他の MCP server・任意 shell command・filesystem/network の安全境界は無効化・緩和しない。

## 方針（最小の product-specific adapter 設定）

`src/runtime/daemon.rs` の `claude_mcp_arguments` に、注入した `usagi` server のツールだけを事前許可する **`--allowedTools mcp__usagi`** を追加する。

- Claude Code 公式仕様では、単一 MCP server の全 tool を無確認許可する正しい構文は **bare server 名 `mcp__usagi`**（`mcp__usagi__*` の wildcard は非対応）。値は server ごとにスコープされ、他 server・Bash・Edit・network 等の permission model には影響しない（`--dangerously-skip-permissions` とは別物）。interactive / headless（`--print`）双方で同じに効く。
- 引数は既存の `--mcp-config` と同様に **ephemeral な `SpawnProvision`** に載せ、durable launch plan / snapshot / IPC response には残さない（credential・raw config・secret を durable state/log/UI に保存しないという制約を維持）。
- `inject_mcp`（= `McpWiring` capability 要求時）のときだけ注入される既存経路をそのまま使う。gating の追加は不要。

## 変更内容

- `src/runtime/daemon.rs`:
  - `claude_mcp_arguments` の戻り値に `"--allowedTools", "mcp__usagi"` を追加する。
  - 既存ユニットテスト `product_mcp_arguments_start_usagi_mcp_from_the_daemon_binary` の Claude 期待値を更新する（Codex 期待値は変更なし）。
- `document/05-daemon.md`: agent ownership 節の MCP 注入の記述に、Claude は注入した usagi server の tool のみを無確認許可し他の tool の permission は維持すること、Codex は `never` approval で既に無確認であることを追記する。

## テスト・確認方法

- `cargo test -p usagi product_mcp_arguments`（root binary crate のユニットテスト）で Claude argv が `--mcp-config … --allowedTools mcp__usagi` を生成することを確認。
- `cargo fmt --all -- --check` / `cargo check --workspace --all-targets` / `cargo clippy --workspace --all-targets -- -D warnings`。
- Markdown link check（docs 差分あり）。
- full test / coverage 100% は PR CI に一本化。`src/runtime/daemon.rs` は `#![coverage(off)]`（Unix socket/process/PTY wiring）だがユニットテストで argv 生成の振る舞いを検証する。

## 非対象

- Codex / 他 agent の approval 挙動の変更。
- 他 MCP server・shell・filesystem・network の permission 緩和。
- `.claude/settings.json` などの durable 設定ファイルの書き出し（既存の inline argv 方式を踏襲し durable state を汚さない）。
