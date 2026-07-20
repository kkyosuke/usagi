---
number: 474
title: fix(daemon): Agent admission を durable-before-spawn にする
status: todo
priority: high
labels: [review, v2, daemon, agent, durability]
dependson: []
related: [271, 321, 322, 383]
parent: 453
created_at: 2026-07-20T12:06:24.378704+00:00
updated_at: 2026-07-20T12:06:24.378704+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/agent_ipc.rs::{AgentRuntime::admit,admit_dispatch}` は `orchestrator.launch` で child を spawn した後に run、operation、binding、agent transition を保存し、ephemeral MCP caller credential を登録する。post-spawn store failure で live orphan/unbound child が残り、最初の MCP call が credential 登録より先に来る race もある。

## 成立条件 / 再現フロー

spawn 後の各 `DispatchStore::upsert_run`、binding/agent transition、caller registration に failpoint を置くか、即時に MCP call する child を起動する。request は失敗しても process は生き、retry/restart が別 child を spawn できる。

## 対象責務と非対象

admission の durable reservation/prepare→spawn→commit または compensating termination、credential availability ordering、idempotent retry を対象とする。snapshot hydrate は #458、session lifecycle reducer は #460、Agent CLI 内部は非対象。

## 受入条件

- [ ] operation/run/binding と credential provenance metadata は spawn 前に durable reservation し、失敗時に child を確実に terminate/reap する transaction を定義する。
- [ ] daemon-minted MCP secret 自体は disk/snapshot/log に永続化せず、in-memory registration を child spawn と最初の MCP call より前に完了する。
- [ ] 各 post-spawn failure、即 exit、retry、restart で同一 operation の spawn count は最大 1。
- [ ] incomplete admission は reconcile 可能で、orphan process と偽 success outcome を残さない。

## 必須回帰テスト

全保存点の failpoint、即終了/即 MCP child、terminate failure、同じ/異なる operation retry、runtime restart を実 PTY composition test で検証する。

## docs / 移行影響

`document/05-daemon.md` に Agent admission transaction、credential ordering、secret 非永続化を追記する。incomplete legacy record は新規 spawn せず unknown/failed として reconcile する。
