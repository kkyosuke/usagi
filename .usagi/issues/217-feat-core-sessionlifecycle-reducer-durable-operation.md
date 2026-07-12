---
number: 217
title: feat(core): SessionLifecycle reducer と durable operation を実装する
status: todo
priority: high
labels: [core, session, lifecycle]
dependson: [214]
related: []
parent: 213
created_at: 2026-07-12T11:38:56.295403+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

[v1 lifecycle proposal](../../v1/document/proposals/05-session-lifecycle.md) の状態機械を v2 の daemon 単一書き手モデルへ移し、session incarnation、長時間操作、late worker を永続状態で fence する。v2 の統合契約は [daemon API proposal](../../document/proposals/04-daemon-api.md#sessioncontrol-api) を正本とする。

## 対象

- `SessionLifecycle`（creating／initializing／available／deleting／failed）と pure reducer／capability。
- `AgentPhase`、`BranchStatus` を lifecycle と直交する別型として導入。
- workspace state envelope、単調 `state_revision`、`SessionId`、attempt、`OperationId`、immutable setup/delete plan。
- operation journal の accepted／running／cancel_requested／succeeded／failed／cancelled／ambiguous と progress revision。
- create/remove/setup crash reconcile、legacy migration barrier、unknown version/state fail-closed。

## 受け入れ条件

- daemon だけが managed session state を書き、TUI／CLI／MCP は reducerやstoreを直に mutation しない。
- completion は `WorkspaceId + SessionId + OperationId + owner DaemonGeneration + execution/lifecycle attempt + expected lifecycle/revision` が一致した場合だけ反映する。
- remove→同名再作成後の旧 worker、逆順 snapshot、実行中 setup の crash が新 record を更新／自動再実行しない。
- lifecycle、AgentPhase、BranchStatus を合成表示できるが、互いの状態値へ畳み込まない。
- transition matrix、crash point、migration、revision race を pure／fake persistence test で網羅する。
