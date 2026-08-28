---
number: 531
title: feat(agent): v2 で agent-phase を daemon へ phase 報告する IPC を配線する
status: done
priority: medium
labels: [agent, daemon, claude, ipc]
dependson: []
related: []
created_at: 2026-07-24T12:39:19.153344+00:00
updated_at: 2026-07-24T21:55:34.911106+00:00
---

## 背景

Claude 起動時の `--settings` フック配線と OS sandbox launcher（`usagi claude-sandbox`）を実装した際、`SessionStart` / `UserPromptSubmit` / `Notification` / `Stop` / `SessionEnd` の各ライフサイクル event と session 起動の `PreToolUse` に `usagi agent-phase <phase>` を配線した。

## やること

- **phase 報告 IPC**: `usagi-core::usecase::client` に phase 報告の `DaemonRequest`（仮 `AgentPhaseReport`）を追加する。`CodexSessionCapture` と同様、`McpCallerContext`（daemon が発行する不透明 credential。env `USAGI_MCP_CALLER_CREDENTIAL` で子へ継承）で報告元の runtime を束縛し、caller は runtime / session / path を名指しできない。
- **`agent-phase` ハンドラ**: `crates/cli/src/cli/hooks/agent_phase.rs` を、stdin の hook payload と credential から phase 報告 request を組み立て、合成ルートの daemon client 経由で送るよう実装する（`codex_session_capture` の `request_from_hook` パターンに倣う）。合成ルート `src/runtime/cli.rs` の `RunOutcome` 配線もそれに合わせる。
- **daemon 側の phase 反映**: `usagi-daemon` の agent runtime に、報告された phase を projection に反映する経路を追加する。既存の `RuntimeState` 由来 phase（[05-daemon.md] 参照）との関係（どちらが優先か、報告 phase を `ProviderResumePhase` にどう写すか）を設計して決める。
- ドキュメント（`document/02-architecture.md` の内部フックコマンド節、`document/05-daemon.md` の phase 節）を更新する。テストは 100% カバレッジ維持。

## 完了条件

- `agent-phase <phase>` が phase を daemon へ報告し、daemon の runtime projection に反映される。
- 報告は daemon 発行 credential で報告元 runtime に束縛され、caller は runtime / session / path を名指しできない。
- credential 不一致・malformed payload は fail-closed で拒否する。

## 参考

- 参考パターン: `usagi-daemon` の Codex `SessionStart` capture（`usecase::agent_ipc::capture_codex_session`）と `DaemonRequest::CodexSessionCapture`

[05-daemon.md]: 05-daemon.md
