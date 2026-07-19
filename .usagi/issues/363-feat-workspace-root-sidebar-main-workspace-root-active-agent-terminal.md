---
number: 363
title: feat(workspace-root): sidebar の main(workspace root) を active にしたまま Agent/Terminal を作成可能にする
status: in-progress
priority: high
labels: [epic, workspace-root, daemon, tui, agent, terminal]
dependson: []
related: []
created_at: 2026-07-19T21:04:31.062133+00:00
updated_at: 2026-07-19T21:05:47.243686+00:00
---

## 目的（Epic）

TUI sidebar の `⌂ root`（workspace root / `main` チェックアウト）を active にした状態でも Agent と Terminal を作成・所有・操作できるようにする。これは session 作成 UI の修正とは独立した目的である。

## 現状（阻害点）

型層は既に root を予期している（`TerminalLaunchScope.session_id` / `TerminalRef.session_id` は `Option<SessionId>`、doc: "absent for a workspace-root terminal"）が、実行時経路が session を必須としているため成立しない。

- **daemon terminal**: `crates/daemon/src/usecase/terminal_ipc.rs` が `scope.session_id == None` を `OwnershipUnknown`（"workspace-root terminal launch is not yet bound to a durable session fence"）で早期拒否。`generic_terminal.rs` の `validate_scope` は `terminal.session_id == Some(operation.session_id)` を要求し、`CompletionFence.session_id` は必須 `SessionId`。
- **daemon agent dispatch**: `Agent` / `LaunchScope` / `CallerRef` / `WorkerRef` / `AgentRuntimeRef`（`terminal.session_id != Some(session)` で `SessionDoesNotOwnTerminal`）/ dispatch inbox（`caller.session_id` で keyed）/ `AgentLaunchIntent.session` がすべて session 必須。
- **composition root**: `SharedTerminalScopeResolver` / `SharedScopeResolver`（`src/runtime/daemon.rs`）が `session_id` 必須で、root（repo root）へ解決する分岐が無い。
- **TUI**: `agent_runtime.rs` の pane host は `HashMap<SessionId, PaneRuntime>` で keyed、`sync_live_pane` が `Target::Root(_) => false`。`controller.rs::submit_closeup` は root の agent 起動を "workspace root cannot start an agent" で拒否。`workspace_runtime.rs::on_effect` は `OpenTerminal`/`LaunchAgent` を `Target::Session` に絞り、root を握り潰す。

## 方針

root を session とは別の**安全な durable scope** として表現し、session scope の隔離と fence を回帰させない。共有の identity/fence/scope 語彙に `session_id: Option<SessionId>` を通し、`None` を workspace root とする（leaf 型の既存方針と一致）。workspace root チェックアウトには **永続化した root `WorktreeId`** を持たせ、snapshot で client に公開する。client からの raw cwd/argv/identity は一切受け付けず、path の権威は daemon の trusted repository root に置く。

設計の正本は [document/proposals/10-workspace-root-scope.md](../../document/proposals/10-workspace-root-scope.md)。

## 完了条件

- `⌂ root` active で Terminal を作成し、出力表示・入力送信・resize・detach/reconnect が動作する。
- `⌂ root` active で Agent を作成し、pane に live terminal が投影され双方向 IO が動作する。
- root scope は client 供給の path/argv/identity を受け付けず、daemon の trusted root に解決する。
- 既存の session scope の隔離・fence を回帰させない（session の launch/attach/exit の回帰テストが green）。
- daemon restart 後も root terminal/agent の trusted root cwd で復元し、ownership generation で fence する。
- 正本ドキュメント（`document/05-daemon.md` / `04-ipc.md` / `03-tui.md` と proposal）を実装契約に更新する。
- coverage 100% を維持する。

## 子 issue

- core: session-optional な scope/fence/dispatch 語彙 + root worktree 永続化
- daemon: root terminal の scope 解決・fencing・restart/reconnect
- daemon: root agent dispatch
- tui: root pane projection と closeup/launch・live IO
- docs: 正本ドキュメント更新
