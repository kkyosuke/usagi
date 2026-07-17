---
number: 323
title: feat(mcp): session_dispatch / session_get / agent_list / agent_get / agent_complete / agent_fail / agent_inbox を実装する
status: todo
priority: high
labels: [mcp, cli, orchestration, agent]
dependson: [321, 322]
related: [97, 106, 109, 110, 146]
parent: 105
created_at: 2026-07-18T00:00:00+00:00
updated_at: 2026-07-18T00:00:00+00:00
---

## 目的

agent 向け dispatch の MCP 契約（7 tool）を daemon IPC client として実装し、caller/run を実行
コンテキストから推論する。既存 tool との互換・移行を正本 docs に反映する。設計の正本は
[document/proposals/08-agent-dispatch-mcp.md](../../document/proposals/08-agent-dispatch-mcp.md)（§3・§5・§7）。

## 背景

v2 の MCP tool は現状 scaffolding（`call()` が `Unimplemented`）で、mutating な `session_*` のみ daemon へ
routing される（`crates/cli/src/mcp/`）。#322 で dispatch runtime と inbox が durable になった前提で、
tool 面を実装する。tool 定義は既存の `Tool` trait（name/description/input_schema）＋ registry に足す。

## やること

- 次の 7 tool を追加・実装する（proposal §3）。配送モード（queue/live）は**公開しない**（常に即時実行）。
  - `session_dispatch { session:{name}, agent:{id} | {runtime,model}, prompt } -> { run_id, session, agent_id }`
    （session upsert、agent 指定は id か runtime+model の排他、id+runtime/model 併用は `InvalidArgument`）。
  - `session_get { name }` — agent 一覧（id/runtime/model/status/現在または最後の task: prompt・開始時刻・状態）。
  - `agent_list { session?, status? }` — id・所属 session・runtime・model・status・task summary・updated_at。
  - `agent_get { agent_id }` — run 履歴・結果要約。
  - `agent_complete { summary, result?, run_id? }` — **宛先引数なし**。caller は保存済み binding から解決。
    run_id は実行コンテキストから推論できれば省略可。`result` は pr/commits/changed_files/verification を構造化。
  - `agent_fail { summary, error?, run_id? }` — 同経路で失敗を配送。
  - `agent_inbox { since?, unread_only? }` — caller 自身の inbox（他 agent からの報告）を取得。親が停止中でも
    次回起動時に読める。
- caller/run の**コンテキスト推論**: MCP サーバは worker の session worktree 内で動くため、実行コンテキストから
  worker の session/agent → current_run → `DispatchBinding` → caller を辿る。曖昧・不一致は `CompletionFence` で
  no-op（proposal §5）。
- registry / `tools/list` の件数テストを更新し、各 tool の input_schema が妥当な JSON であることを検証する。
- 互換・移行を正本へ反映する（proposal §7 の表）:
  [02-architecture.md](../../document/02-architecture.md) の入口面 MCP tool dispatch と
  [01-entry-surfaces.md](../../document/proposals/01-entry-surfaces.md) / [04-ipc.md](../../document/04-ipc.md)。
  `session_delegate_brief` / `session_delegate_issue` / `issue_to_prompt` / `session_prompt` /
  `session_complete` は**併存**（置き換えない）ことを明記する。

## 受け入れ条件

- root ガード（#106）と両立し、dispatch/可視化 tool が daemon IPC 経由で動作する。
- `session_dispatch` が session を upsert し、id 指定は既存 agent 再利用・runtime+model 指定は新規作成、
  併用はエラー、成功時に `run_id` を返す。
- `session_get` / `agent_list` / `agent_get` が proposal §3 の shape を返す。
- `agent_complete` / `agent_fail` が宛先引数なしで caller inbox へ配送し、`run_id` 省略時はコンテキスト推論する。
- `agent_inbox` が親停止中に届いた `NoReport` を含む報告を取得できる。
- 既存 delegate/prompt/complete tool の挙動と互換性が保たれ、移行方針が正本 docs に記載される。
- カバレッジ 100%。

## 非目標

- daemon runtime / store の実装（#321 / #322）。
- TUI からの dispatch 表示・操作（別 issue）。

## テスト方針

- `cargo test -p usagi-cli mcp`
- push/PR 前は full gate（coverage 100%）と Markdown link check。
