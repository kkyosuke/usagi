---
number: 328
title: feat(mcp): supervisor の control・観測 API と既存 session MCP の移行境界を実装する
status: todo
priority: high
labels: [mcp, cli, orchestration, supervisor, docs]
dependson: [323, 327]
related: [97, 106, 109, 110, 182, 183, 187, 329, 330]
parent: 324
created_at: 2026-07-17T21:12:50.059083+00:00
updated_at: 2026-07-17T21:12:50.059083+00:00
---

## 目的

durable supervisor を MCP から開始・観測・cancel・human escalation 解決できるようにし、既存 session_* MCP と daemon Orchestrator の責務境界、後方互換性、移行を実装済み仕様として確定する。#323 の agent/session 観測と dispatch/inbox を置き換えず、その上位の run-level API を追加する。

## MCP 契約

daemon IPC client として次を実装する。名前・JSON schema・wire shape はこの issue の受け入れテストで固定する。

| tool | 入力 | 結果 |
| --- | --- | --- |
| supervisor_start | root task、initial task DAG?、policy selector? | supervisor_run_id、root task、state revision |
| supervisor_get | supervisor_run_id、event cursor? | run state、DAG node summary、policy summary、provenance summary、pending escalation、next action reason |
| supervisor_list | state?、caller/session? | paginated run summaries |
| supervisor_cancel | supervisor_run_id、reason | fenced cancel operation / current state |
| supervisor_resolve_escalation | supervisor_run_id、escalation id、authorized decision | new revision/state。resume/cancel/fail のみ |
| supervisor_events | supervisor_run_id、after sequence | ordered durable event summaries / cursor |

- start は root caller provenance を実行コンテキストから保存し、同一 operation/idempotency key の再送では同じ run を返す。client supplied session path、agent argv、secret、未検証の caller identity は受理しない。
- get/list/events は #323 の session_get、agent_list、agent_get が返す agent/session/run 観測と整合し、task→dispatch run→session/agent を参照できるが、prompt 本文・secret・raw argv・credential は返さない。
- cancel/escalation resolution は #327 の policy/authority/fence を通る。root/任意 worker が escalation を勝手に解除できない。

## 既存 API との責務境界・移行

| surface | 継続する責務 | supervisor との関係 |
| --- | --- | --- |
| session_create / session_* lifecycle | session/worktree の create/list/remove と状態 | supervisor は daemon の stable scope resolver を消費し、lifecycle の書き手にならない |
| session_dispatch / agent_* / agent_inbox（#323） | 1 worker の即時起動、agent/run 観測、caller inbox | supervisor scheduler が内部 effect として使う。単発利用を維持する |
| session_delegate_brief / session_delegate_issue | 人間/root 主導の session 起源・issue 遂行 | 維持。supervisor はこれらを暗黙に置換・再実装しない |
| session_prompt / session_complete | 明示 prompt 配送と自由文通知 | 維持。supervisor の fenced wake/event の正本にしない |
| daemon Orchestrator | durable state、effect reservation、scheduler、fence、policy | 唯一の supervisor writer。MCP は client/adapter のみ |

- feature rollout は opt-in の supervisor_* tool の追加から始め、既存 tool の名前・入力・既定挙動を変更しない。
- 旧 #182/#183 orchestration plan と supervisor run は read/write store を共有しない。移行は新規 run の opt-in のみで、必要な相関は read-only related link に限る。
- 実装完了時に限り、document/02-architecture.md、04-ipc.md、05-daemon.md、該当 command/MCP doc に責務表、API、migration、運用・テスト手順を反映する。未実装事項は proposal/issue に残し、正本仕様に混在させない。

## テスト戦略

- CLI/MCP: tools/list、schema validation、unknown/forbidden field、idempotent start、cursor pagination、safe redaction、root guard。
- daemon IPC: start→dispatch→completion→wake→verification→success、failure/no-report→retry/escalation、cancel、restart/reconnect、late/duplicate event。
- compatibility: 既存 session_delegate_*、session_prompt、session_complete、#323 tool の input/output/behavior が不変であること。
- docs: 実装済み Markdown の link/anchor check を実行する。

## 受け入れ条件

- supervisor の開始、状態/DAG/provenance/event 観測、cancel、authorized escalation resolution が daemon IPC を通じて durable に動く。
- #323 の agent/session 観測を利用して run 詳細を相関でき、既存 MCP contract は破壊しない。
- root guard と caller provenance が維持され、非 authorized caller は cancel/escalation resolution を実行できない。
- docs は実装済みの API/責務だけを正本へ記載し、Markdown link check と coverage 100% を通す。

## 非目標

- supervisor 専用 TUI。
- 任意の既存 session を自動的に supervisor run に採用する移行。
- agent 起点の `user_decision_request` と choice/freeform 回答の transport。これは #329 の契約とし、
  本 issue の `supervisor_resolve_escalation` を暗黙に流用しない。
