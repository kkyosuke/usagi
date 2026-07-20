# 設計提案（proposals）

> [ドキュメント目次](../README.md)

`document/` 直下の番号付きドキュメント（`01-` …）は**現在のビルドで動作する仕様の正本**であり、
[06-conventions.md#記載実装済み](../06-conventions.md#記載実装済み) に従って未実装の内容を含めない。

一方、まだ実装されていない**構成・機構の設計判断**を記録したいことがある。これを spec に混ぜると
「どこまで本当か」が読者に判断できなくなるため、**設計提案はこの `proposals/` に分離**する。実装が進んで
挙動が確定したら、その内容を正本（`02-architecture.md` など）へ畳み込み、提案は撤去またはリンクだけ残す。
ロードマップ（実装タスク）は issue ストア（`.usagi/issues/`）で追跡する。

v1 時点の設計提案（daemon 化・durable orchestrator など）は退避版
[v1/document/proposals/](../../v1/document/proposals/README.md) にあり、更新しない。そこにある
[daemon 化提案](../../v1/document/04-orchestration.md)の実装済み部分は、[TUI](../03-tui.md)、
[daemon IPC](../04-ipc.md)、[daemon](../05-daemon.md)へ畳み込んだ。退避版を変更して stub にせず、
v1 の仕様スナップショットとして保存する。

## 一覧

| # | ドキュメント | 内容 | 状態 |
|---|---|---|---|
| 1 | [01-entry-surfaces.md](01-entry-surfaces.md) | 入口面（CLI / MCP）の配置と、daemon を実行の権威とする反映フロー | 提案（クレート構成・dispatch は [02-architecture.md](../02-architecture.md) へ畳み込み済み） |
| 2 | [02-ipc-id.md](02-ipc-id.md) | v2 daemon IPC の目標・権威・typed ID・fencing invariant | [04-ipc.md](../04-ipc.md) へ畳み込み済み |
| 3 | [03-ipc-protocol.md](03-ipc-protocol.md) | envelope、handshake、stream、idempotency、bounded transport、error | [04-ipc.md](../04-ipc.md) へ畳み込み済み |
| 4 | [04-daemon-api.md](04-daemon-api.md) | terminal/session command・event と socket/workspace/launch security | [04-ipc.md](../04-ipc.md) / [05-daemon.md](../05-daemon.md) へ畳み込み済み |
| 5 | [05-daemon-lifecycle.md](05-daemon-lifecycle.md) | active/draining restart、crash orphan、配置、実装 issue、test strategy | [05-daemon.md](../05-daemon.md) へ畳み込み済み |
| 6 | [06-tui-v1-parity.md](06-tui-v1-parity.md) | v2 TUI の parity scope・優先度・受け入れ契約 | 提案 |
| 7 | [07-pty-crash-continuation.md](07-pty-crash-continuation.md) | PTY broker／FD handoff による daemon crash 後の terminal 継続 | 提案（MVP 非依存） |
| 8 | [08-agent-dispatch-mcp.md](08-agent-dispatch-mcp.md) | 他 session の特定 agent への即時 dispatch、runtime/model validation、caller の durable inbox への確実な完了報告（MCP 契約） | 提案（実装 issue #321–#323, #331–#332） |
| 9 | [09-user-decision-mcp.md](09-user-decision-mcp.md) | agent の user decision request と durable な回答配送・TUI 操作 | 提案（実装 issue #329–#330） |
| 10 | [10-workspace-root-scope.md](10-workspace-root-scope.md) | workspace root（`⌂ root`）で Agent/Terminal を作成する session-optional な scope/fence 設計 | [04-ipc.md](../04-ipc.md) / [05-daemon.md](../05-daemon.md) / [03-tui.md](../03-tui.md) へ畳み込み済み（実装 issue #363–#368） |
| 11 | [11-workspace-restore-panes.md](11-workspace-restore-panes.md) | workspace open 時に scope 内の live Agent/Terminal を daemon inventory から pane tab へ復元する設計 | 提案（実装 issue #390 / #386 / #388） |
