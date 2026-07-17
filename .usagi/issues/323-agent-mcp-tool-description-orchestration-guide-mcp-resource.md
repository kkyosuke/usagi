---
number: 323
title: agent 向け MCP 利用導線の改善（tool description 拡充 + orchestration guide を MCP Resource で公開）
status: done
priority: medium
labels: [mcp, docs]
dependson: []
related: []
created_at: 2026-07-17T20:59:35.824372+00:00
updated_at: 2026-07-17T21:59:12.628349+00:00
---

## 背景 / 目的

`usagi mcp`（stdio JSON-RPC）は agent 向けの入口面で、`tools/list` で全 tool を公開している。しかし現状は次の課題がある。

- 各 tool の `description` が「何をするか」の 1 行止まりで、agent が `tools/list` の一覧から「いつ使うか」「主な制約・前提」を判断しづらい。
- orchestration（session dispatch・観測・完了報告）の使い方をまとめて参照できる導線が MCP 上に無い。設計提案 [proposals/01-entry-surfaces.md](../../document/proposals/01-entry-surfaces.md) はあるが agent が実行時に読める形になっていない。
- MCP は `initialize` / `ping` / `tools/list` / `tools/call` のみ対応で、`resources/*`（MCP Resource）を持たない。

## やること

1. **tool description の拡充**（`crates/cli/src/mcp/tools/{issue,memory,session}.rs`）
   - 各 tool の `description()` に「いつ使うか」「何をするか」「主な制約・前提」を簡潔・正確に記載する。
   - **tool 名・`input_schema`（wire 契約）は変更しない**。description 文字列のみ改善する。

2. **MCP Resource 機構の追加**（`crates/cli/src/mcp/`）
   - `resources/list` / `resources/read` を serve ループに追加し、`initialize` の capabilities に `resources` を宣言する。
   - resource は静的テキスト（uri / name / description / mimeType / text）として registry で管理する（tool registry と同じ presentation 規律）。

3. **orchestration guide の公開**
   - URI `usagi://guides/orchestration` で orchestration の利用ガイドを公開する。
   - registry に存在する実在の tool 名だけを使い、dispatch（`session_delegate_issue` / `session_delegate_brief`）・観測（`session_list` / `session_status` / `session_pr`）・完了報告（`session_complete`）のワークフロー、代表例、状態遷移、制約を説明する。
   - 未実装の phantom tool（`session_dispatch` / `agent_list` 等）は記載しない。

4. **テスト**
   - resource discovery（`resources/list`）と read（`resources/read`）のテストを追加する。resource lookup ロジックは covered な純関数に置き、serve 側は薄い glue に保つ。
   - `resources/list` を未知 method として扱っていた既存テストを更新する。

5. **ドキュメント**（v2 正本）
   - `document/07-mcp.md` を新設し、MCP 入口・tool 面・resource 機構・orchestration guide の URI を記載する。目次（`document/README.md`）に追加する。

6. **agent 起動プロンプト**
   - 起動プロンプトへ大きな説明文を注入しない。必要なら短い案内を resource 参照へ向ける程度に留める。

## 完了条件

- `tools/list` の全 tool description が「いつ / 何を / 制約」を含む。
- `resources/list` が `usagi://guides/orchestration` を返し、`resources/read` がその本文を返す。
- resource discovery/read のテストが通り、coverage 100% を維持する。
- v2 正本ドキュメントが実装に追随している。
- 未実装機能を実装済みとして記載していない。
