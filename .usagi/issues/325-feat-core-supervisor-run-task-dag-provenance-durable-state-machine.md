---
number: 325
title: feat(core): supervisor run・task DAG・provenance の durable state machine を追加する
status: done
priority: high
labels: [core, orchestration, supervisor, durable, dag]
dependson: [321]
related: [183, 219, 322, 323]
parent: 324
created_at: 2026-07-17T21:11:47.376995+00:00
updated_at: 2026-07-17T21:50:05.960480+00:00
---

## 目的

#324 の基盤として、supervisor run、task DAG、親子 run provenance、状態遷移、event journal を core の durable reducer/store として実装する。daemon の scheduler、MCP tool、policy の解釈は本 issue に含めない。

## データモデル

- SupervisorRun: UUIDv7 の supervisor_run_id、root caller ref、root task/input digest、policy revision、state revision、created/updated/terminal timestamps、terminal reason を持つ。
- TaskNode: stable task_id、supervisor_run_id、parent_task_id?、dependency task IDs、task instruction digest/body、required artifact contract、attempt/generation、assigned dispatch run?、state を持つ。task_id は親子の path/provenance を失わない opaque ID とする。
- RunProvenance: supervisor_run_id、task_id、parent task/run?、dispatch run_id、worker session/agent/worktree incarnation を one-to-one に fence し、同名 session や retry の混同を許さない。
- SupervisorEvent: monotonic event sequence、causation/correlation IDs、observed_at、payload digest、source（dispatch completion/failure/no-report、timer、cancel、verification）を durable append する。再読・重複 event を idempotently reducer に適用できること。
- 状態は SupervisorRun（Planning / Running / WaitingForDecision / Verifying / Succeeded / Failed / Cancelled / Escalated）、TaskNode（Pending / Ready / Dispatched / Running / AwaitingDecision / Retrying / Verifying / Succeeded / Failed / Cancelled / Blocked）を最小集合とし、terminal state を再開しない。詳細な policy 判断は #327 が与える。

## やること

- crates/core の domain に typed ID、entity、event、transition error、snapshot/query model を追加する。domain は既存依存制約を守る。
- durable store を daemon state dir に追加し、snapshot + append-only event journal を atomic write / lock / sequence CAS で保存する。partial write、restart、同一 event 再配送、stale generation を安全に扱う。
- pure reducer を実装する。許可されない遷移、dependency 未達の dispatch、parent/child provenance 不一致、terminal 後 mutation を typed error/no-op として定義する。
- task DAG の cycle/self-edge を受理前に拒否し、dependency が全て succeeded の task だけを Ready に投影する。DAG への追加は run が terminal でない間だけ許可する。
- query API で supervisor run、task、provenance、event cursor を取得できるようにする。返却に prompt 本文・secret・raw runtime argv を含めない。

## 受け入れ条件

- crash/restart 後に同じ snapshot/event journal から同じ Ready node と provenance を再構成する。
- duplicate/out-of-order event、stale task generation、dispatch run の再送が task を二重 terminal 化・二重委譲しない。
- parent task、child task、dispatch run、session、agent を run history で機械的に辿れる。
- cycle、未達 dependency、terminal mutation、不正 provenance は durable state を壊さず安全に拒否される。
- unit/store crash-injection test で lines/functions 100% を維持する。

## 非目標

- worker の起動・wake/restart・timer loop（#326）。
- budget/retry/cancel/escalation/verification の policy 解釈（#327）。
- MCP exposure と既存 API の移行（#328）。
