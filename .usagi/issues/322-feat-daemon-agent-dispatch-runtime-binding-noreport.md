---
number: 322
title: feat(daemon): agent dispatch を launch runtime へ接続し caller↔worker binding と「報告なし」検知を実装する
status: todo
priority: high
labels: [daemon, mcp, orchestration, agent, ipc]
dependson: [321]
related: [268, 271, 109, 110]
parent: 105
created_at: 2026-07-18T00:00:00+00:00
updated_at: 2026-07-18T00:00:00+00:00
---

## 目的

#321 の durable ドメイン／store を daemon の Agent launch runtime に接続し、**session upsert →
agent 解決 → prompt 即時 launch → run/binding 永続化 → run_id 返却**と、**PTY exit 時の「完了報告なし」
検知**を実装する。設計の正本は
[document/proposals/08-agent-dispatch-mcp.md](../../document/proposals/08-agent-dispatch-mcp.md)（§5・§6）。

## 背景

daemon は既に durable runtime（reservation → snapshot → spawn → journal → exit）と `CompletionFence`、
`LaunchRequest.initial_prompt` を持つ（`crates/daemon/src/usecase/runtime.rs` / `agent_ipc.rs`）。
一方 dispatch を成立させ caller↔worker を結ぶ経路と、報告漏れの検知は無い。#268 の scope resolver が
`available` と証明した session scope の上でのみ launch する原則（#271）を踏襲する。

## やること

- `DaemonRequest` に dispatch 用の action / intent を追加する（session upsert・agent 指定・prompt を運ぶ）。
  既存 `DaemonRequest::Agent` / `AgentLaunchIntent` を拡張するか隣接 variant を足すかは実装時に決め、
  proposal §7 の「合成」方針（新実行ロジックを二重に持たない）を守る。
- dispatch 受理時に:
  - `session.name` を upsert（既存 lifecycle を再利用。無ければ create、在れば再利用）。
  - agent を **id 指定なら既存 `Agent` を解決**、**runtime+model 指定なら新規 `Agent` を作成**（併用は typed error）。
  - `LaunchRequest.initial_prompt` に prompt を載せて**即時 launch**（queue/live を選ばせない）。
  - `DispatchRun`（run_id = launch operation の `OperationId`）と `DispatchBinding{run_id, caller, worker}` を
    #321 の store に永続化し、`run_id` を返す。caller は dispatch 実行コンテキストから機械的に記録する。
- **「報告なし」検知**: 既存の PTY exit commit（`runtime.exit` / `agent_ipc.exit`）で、当該 run_id に
  `Completed`/`Failed` の inbox 配送がまだ無ければ、runner が `NoReport` の `InboxMessage` を caller へ
  合成配送する。late/duplicate/wrong-generation は `CompletionFence` で照合し二重配送しない。
- fake store / injected PTY による deterministic な integration test（dispatch→launch→exit→NoReport、
  complete 済みなら NoReport を出さない、fence 不一致は no-op）。

## 受け入れ条件

- dispatch は #268 が `available` とした session scope でのみ受理され、それ以外は typed safe error で
  PTY を spawn しない。
- id 指定は既存 agent を再利用し、runtime+model 指定は新規 agent を作る。id と runtime/model の併用はエラー。
- 成功 dispatch は run/binding を永続化し `run_id` を返す。同一 operation の再送は同じ結果を返す（idempotent）。
- worker が complete/fail せず exit した run は `NoReport` が caller inbox に必ず届く。
- 実装済みの IPC/daemon 契約を [04-ipc.md](../../document/04-ipc.md) / [05-daemon.md](../../document/05-daemon.md) の正本へ反映する。
- カバレッジ 100%。

## 非目標

- MCP tool 層の実装（#323）。
- queue/live 配送の再設計（`session_prompt` の挙動は変えない）。
- daemon crash 後の PTY FD 継続（proposals/07）。

## テスト方針

- `cargo test -p usagi-daemon usecase::agent_ipc`
- `cargo test -p usagi-daemon usecase::runtime`
- push/PR 前は full gate（coverage 100%）。
