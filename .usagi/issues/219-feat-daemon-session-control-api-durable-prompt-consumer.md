---
number: 219
title: feat(daemon): session/control API と durable prompt consumer を実装する
status: done
priority: high
labels: [daemon, session, orchestration]
dependson: [216, 217, 218]
related: []
parent: 213
created_at: 2026-07-12T11:39:24.399363+00:00
updated_at: 2026-07-12T20:45:56.871474+00:00
---

## 目的

session snapshot/subscribe、phase ingestion、create/remove/setup、prompt delivery、long operation progress/cancel/reconcile を daemon authority の一つの control surface として実装する。設計は [session/control API](../../document/proposals/04-daemon-api.md#sessioncontrol-api) を正本とする。

## 対象

- revision 付き workspace/session snapshot と resume/resync subscription。
- `AgentRuntimeId` ごとの phase report sequence／capability token／pure reducer。`SessionLifecycle`・`AgentPhase`・`BranchStatus` は別 field。
- typed `SessionCreate`／`SessionRemove`／setup plan と `OperationAccepted`／progress／cancel／get/reconcile。
- prompt `queued → claimed → terminal_reserved → input_acknowledged → running` transaction、bounded retry/backoff/dead-letter。
- autostart concurrency reservation、typed agent launch resolution、terminal-only pane と複数 Agent pane の識別。
- bounded RequestId response cache、producer-issued OperationIdのdurable journal、owner generation/execution attemptとcrash recovery。

## 受け入れ条件

- TUI 不在でも daemon が queue/autostart と session operation を進め、managed pathで client がローカル実行へ fallback しない。
- create/remove/setup/prompt の response loss・daemon restart・late workerでも同一 intent を二重実行せず、ambiguous side effect は自動再実行しない。
- non-available session に通常 spawn/promptを配送せず、remove開始後に新しい queue/spawn reservationをcommitしない。
- phase report は対象 runtime・session incarnation・terminal generation・source sequence を検証し、別 Agent の phaseを上書きしない。
- prompt targetを省略したときeligible runtimeが複数なら、明示primary policyがない限り`ambiguous_target`として配送しない。
- cancellation は request受理と完了を分け、timeout/disconnectだけでoperationをcancel扱いしない。
- reducer／fake store／crash injection／daemon+PTY E2Eで競合、ACK loss、limit、retry、reconcileを検証する。
