---
number: 72
title: feat(mcp): MCP 経由のセッション作成失敗もエラーログに記録する
status: done
priority: medium
labels: [feat, mcp, error-log]
dependson: []
related: [71]
created_at: 2026-06-21T00:00:02.000000+00:00
updated_at: 2026-06-21T03:23:20.664099+00:00
---

## 背景

error-log PR #236 が記録するのは TUI 経由のセッション失敗のみ。`src/presentation/mcp/session.rs` の `tool_create` は `session::create(...).map_err(|e| e.to_string())?` で、失敗を MCP クライアントへ返すだけで `ErrorLog` には書き出さない。

エージェントが MCP の `session_create` でセッションを作って失敗した場合、エラーは呼び出し元 AI に返るだけで usagi 側のログには残らず、後から横断的に追跡できない。

## やること

- MCP の `session_create`（および同様にエラーを返す MCP ツール）が失敗したとき、クライアントへ返すのに加えて `ErrorLog::record` で記録する。
  - 記録例: `"mcp session_create \"<name>\" failed: <chain>"`。
- TUI 側の記録メッセージ書式（`session create "<name>" failed: ...`）と整合を取る。#71 の単一シンクが入る場合はそれを利用する形で重複を避ける。
- MCP プロセスはヘッドレスで動くため、`ErrorLog::open_default` のデータディレクトリ解決が TUI と同一になることを確認する。

## 確認方法

- MCP `session_create` を意図的に失敗させ（例: 既存名・不正名）、`~/.usagi/logs/` に記録されること。
- 成功時はログに残らないこと。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
