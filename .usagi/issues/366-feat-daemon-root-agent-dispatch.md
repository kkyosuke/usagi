---
number: 366
title: feat(daemon): root agent dispatch を接続する
status: done
priority: high
labels: [daemon, workspace-root, agent, ipc]
dependson: [364, 365]
related: []
parent: 363
created_at: 2026-07-19T21:05:11.789266+00:00
updated_at: 2026-07-19T22:12:23.314432+00:00
---

## 目的

workspace root scope（`session_id: None`）で agent を dispatch/launch できるよう、daemon の admission 経路を session-optional 対応にする。session agent の所有・fence は回帰させない。

## 変更内容

- `crates/daemon/src/usecase/agent_ipc.rs`
  - `dispatch` / `admit` / `admit_dispatch` を `Option<SessionId>` scope 対応に。root では worker 所有検証・`TerminalRef`/`AgentRuntimeRef`/`CompletionFence` を `session_id: None` で構築。
  - scope 解決を root 対応（root → trusted repository root、root worktree id）。
- `src/runtime/daemon.rs`
  - `SharedScopeResolver` に root 分岐（`session_id: None` → `repository_root()`）。

## 完了条件

- root scope の agent launch が accepted→succeeded で live terminal を返し、operation replay/reconnect が動作する。
- session agent dispatch（worker 所有・inbox・fence）の回帰テストが green。
- root agent の inbox が予約キーに分離される。
- coverage 100%。

## 依存

#364（core 語彙）、#365（terminal scope 解決基盤）。
