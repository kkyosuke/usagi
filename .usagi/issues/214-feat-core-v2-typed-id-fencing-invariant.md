---
number: 214
title: feat(core): v2 typed ID と fencing invariant を導入する
status: todo
priority: high
labels: [core, ipc, domain]
dependson: [212]
related: []
parent: 213
created_at: 2026-07-12T11:38:17.560741+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

path、表示名、PID、daemon 内 counter を identity として使わず、v2 の全 resource を incarnation と ownership generation で fence する。設計は [v2 IPC／ID proposal](../../document/proposals/02-ipc-id.md#id-体系と不変条件) を正本とする。

## 対象

- `WorkspaceId`、`SessionId`、`WorktreeId`、`TerminalId`、`AgentRuntimeId`
- `ClientId`、`ConnectionId`、`RequestId`、`OperationId`
- `DaemonGeneration`、`ProtocolVersion`（generation／revision）
- `TerminalRef` と workspace→session→worktree→terminal→runtime の所有 scope
- ID の parse／serde／validation、発行・永続化・廃棄規則

## 不変条件

- session remove→同名再作成、workspace unregister→再登録、worktree 再構築では新 ID を発行し、旧 ref を alias しない。
- terminal command は `DaemonGeneration + TerminalId + WorkspaceId + SessionId? + WorktreeId` が全一致したときだけ適用する。
- Agent pane ごとに `AgentRuntimeId` を発行し、worktree 単位 phase で複数 Agent を混同しない。
- late worker の完了は `SessionId + OperationId + owner DaemonGeneration + execution/lifecycle attempt + expected revision` 不一致なら no-op として記録する。
- name／path は検索・表示値に留め、effecting command の resource key にしない。

## テスト

- newtype の round-trip／invalid input／型取り違え compile boundary。
- 同名・同 path 再作成後の stale ref、別 workspace、別 generation、複数 Agent runtime を表駆動で拒否する pure test。
- legacy record migration は unknown／ambiguous identity を通常 resource として扱わず fail-closed にする。
