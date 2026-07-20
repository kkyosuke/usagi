---
number: 458
title: fix(daemon): Agent runtime と operation ledger を restart 時に hydrate する
status: todo
priority: high
labels: [review, v2, daemon, agent, durability]
dependson: []
related: [209, 251, 271, 283, 311, 350, 386, 402, 460]
parent: 453
created_at: 2026-07-20T12:06:19.000658+00:00
updated_at: 2026-07-20T12:35:29.639277+00:00
---

## 問題・影響

root/v2 の `src/runtime/daemon.rs` にある `FileRuntimeStore::reconcile_after_restart` は `agents.json` を `identity_unknown` に書き換えるだけで、`open_agent_runtime` が作る `AgentRuntime` の coordinator、operation ledger、outcome を hydrate しない。同じ `OperationId` の再送が replacement/double spawn になり、最初の新規 launch が旧 snapshot を上書きし、完了 outcome の replay も消える。

## 成立条件 / 再現フロー

Agent launch/dispatch を受理して snapshot を保存し daemon を再生成する。その後同じ operation を再送、outcome を照会、別 Agent を launch すると、fresh な `AgentRuntime::with_dispatch_and_locator` の in-memory map に旧状態がないため既存処理を再実行または消失させる。

## 対象責務と非対象

`agents.json` の load/reconcile、Agent coordinator、semantic operation key、safe outcome、binding/generation の保守的 hydrate を対象とする。OS 再起動後に旧 PTY を再接続する FD handoff と credential 永続化は非対象で、credential は再起動時も ephemeral/fail closed とする。generic terminal は #459。

## 受入条件

- [ ] snapshot load を spawn admission より先に完了し、失敗・未知 schema・破損時は spawn と上書きを禁止する。
- [ ] 同一 operation/intent は再 spawn せず、成功・非 0 exit・safe failure の durable outcome を同じ意味で replay する。
- [ ] 同一 operation の異なる intent は conflict、`identity_unknown` は `live: false` かつ制御不可として投影する。
- [ ] 新しい launch が hydrate 済みの旧 record/outcome を保持したまま snapshot を更新する。

## 必須回帰テスト

実ファイルを共有する daemon runtime 2 instance の restart integration test で、dispatch retry の spawn count 1、outcome replay、異 intent conflict、旧 snapshot 非上書き、破損 snapshot fail closed を検証する。

## docs / 移行影響

`document/04-ipc.md` と `document/05-daemon.md` に restart/replay と `identity_unknown` の契約を記載する。旧 snapshot は成功を捏造せず非 spawnable state へ保守的に移行する。
