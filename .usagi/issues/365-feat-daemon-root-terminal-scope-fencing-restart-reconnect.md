---
number: 365
title: feat(daemon): root terminal の scope 解決・fencing・restart/reconnect
status: done
priority: high
labels: [daemon, workspace-root, terminal, ipc]
dependson: [364]
related: []
parent: 363
created_at: 2026-07-19T21:05:05.343203+00:00
updated_at: 2026-07-19T22:12:21.320260+00:00
---

## 目的

`session_id: None` の generic terminal launch を daemon が安全に受理し、trusted repository root を cwd として PTY を起動・所有できるようにする。session terminal の fence は回帰させない。

## 変更内容

- `crates/daemon/src/usecase/terminal_ipc.rs`
  - `Launch` arm の `session_id == None` 早期拒否を撤去。`TerminalRef` / `CompletionFence` を scope の `Option<SessionId>` から構築。
- `crates/daemon/src/usecase/generic_terminal.rs`
  - `validate_scope` を `terminal.session_id == operation.session_id`（Option 同士）へ。
- `crates/daemon/src/usecase/session_runtime.rs`
  - root scope 解決（`workspace_id` 照合 → repo root path、永続化済み root `WorktreeId` 検証）を追加。snapshot に root worktree id を公開。
- `src/runtime/daemon.rs`
  - `SharedTerminalScopeResolver` に root 分岐（`session_id: None` → `repository_root()`、要求 worktree_id が daemon の root worktree と一致することを検証）。restart 後も trusted root cwd を使用。

## 完了条件

- root scope の launch/attach/output/input/resize/detach/reconnect/exit が動作する。
- client 供給の path を受け付けず、cwd は必ず trusted repository root。
- session terminal の scope 検証・fence の回帰テストが green。
- restart 後、復元済み root terminal が trusted root で fence される。
- coverage 100%。

## 依存

#364（core 語彙）。
