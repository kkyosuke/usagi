---
number: 386
title: feat(daemon): terminal inventory を agent runtime terminal も含む scope-filtered な unified inventory にする
status: done
priority: high
labels: [daemon, terminal, agent, ipc, restore]
dependson: []
related: [195, 254, 350, 365, 367]
parent: 390
created_at: 2026-07-20T01:42:10.613495+00:00
updated_at: 2026-07-20T03:32:02.132279+00:00
---

## 目的

`terminal inventory` request が、generic terminal だけでなく **Agent runtime が所有する terminal も含めて**、要求 scope（workspace / session / root）に属する daemon-owned runtime を列挙できるようにする。これは #390（restore-on-open）で TUI が open 時に tab を再投影するための source of truth を提供する。

親: #390。

## 現状の問題

- `TerminalRequest::Inventory` は `TerminalRef` を持たないため、`SharedTerminalOwner`/`handle_terminal` が `NotOwned` を返し（`crates/daemon/src/usecase/agent_ipc.rs`）、inventory は generic terminal coordinator だけが応答する（`terminal_ipc.rs`）。**Agent runtime terminal は inventory に現れない**。
- 返る `TerminalInventory { terminal, live }` は kind（agent / terminal）を持たず、Agent tab として投影するのに必要な public 表示情報が無い。

## 変更内容

- **routing**: `Inventory` を shared owner 経由で agent owner と generic owner の両方に fan-out し、結果を merge する。`SharedTerminalOwner` が `Inventory` を `NotOwned` にしないよう、両 coordinator の inventory を集約する経路を追加する（`agent_ipc.rs` / `terminal_ipc.rs` / `crates/daemon/src/presentation/ipc.rs`）。
- **scope filter**: 要求 `scope`（`WorkspaceId` + `Option<SessionId>`（None=root）+ `WorktreeId`）に完全一致する runtime だけを返す。別 workspace / 別 worktree / 別 session は除外する。root scope の解決は #365 の root scope 契約（`session_id: None` → trusted repository root、daemon 公開の root worktree id 照合）に従う。
- **item schema**: inventory item に次を持たせる（core `TerminalInventory` を拡張、または新型）。
  - 完全な `TerminalRef`（fencing 用: `daemon_generation` / `terminal_id` / `workspace_id` / `session_id?` / `worktree_id`）。
  - kind: `agent` | `terminal`。
  - liveness: 現 generation が所有し attach 可能な `live` か否か。
  - agent の場合、Agent tab 表示に必要な public 情報（public launch plan snapshot 由来の profile 表示など）だけ。**argv / environment 値 / secret / provider transcript は含めない**（#254 / #253 の redaction 契約を維持）。
- **liveness 判定**: 現 daemon generation が所有し `TerminalState::Available` 相当のものだけを `live: true`。`exited` / `ReconcileRequired` / `OrphanRunning` / `IdentityUnknown` は inventory に出す場合でも `live: false`（attach 不可）とし、決して attachable として返さない。restore-on-open が誤って live tab を作らないための不変条件。

## 完了条件

- 同一 daemon 上に root scope と複数 session scope の Agent / Terminal runtime がある状態で、`inventory{scope}` が該当 scope の全 live runtime を kind 付きで返し、scope 外を返さない。
- Agent runtime terminal が inventory に現れ、その `TerminalRef` で `attach` / `resume` / `resync` が成功する。
- exited / orphan / identity_unknown は `live: false`（または非列挙）で、attachable として返らない。
- generic terminal の既存 inventory / attach / fence の回帰テストが green。
- IPC payload / durable snapshot / wire event に argv / secret / transcript が現れない。
- coverage 100%。

## テスト方針

- **fake daemon inventory**: agent + generic の両 coordinator を持つ fixture で、root + 複数 session scope の混在 runtime に対する scope filter・kind 付与・live/non-live 分類を検証する（`crates/daemon/src/usecase/*` の in-memory `RuntimeStore` / `TerminalStore` fake、`agent_ipc.rs` / `terminal_ipc.rs` の test 経路を再利用）。
- **durable store round-trip**: 拡張した inventory item schema の (de)serialize と後方互換（未知/欠損フィールドで失敗しない）。
- **fence 回帰**: 古い generation・別 scope の runtime が inventory に混ざらない／attachable にならない。
- **redaction**: inventory response に secret / argv / transcript が無いことを固定する。

## 依存

無し（既存 durable store / coordinator の上に構築）。#390 の前段。
