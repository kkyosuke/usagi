---
number: 324
title: feat(orchestration): durable supervisor event loop を導入する
status: todo
priority: high
labels: [orchestration, supervisor, epic, daemon, agent]
dependson: []
related: [182, 183, 187, 219, 321, 322, 323]
created_at: 2026-07-17T21:11:29.509875+00:00
updated_at: 2026-07-17T21:13:12.397717+00:00
---

## 目的（Epic）

agent が task DAG を自動分解して worker へ委譲し、durable な完了／失敗報告を回収して次の判断を繰り返す **durable supervisor event loop** を導入する。単発 dispatch と inbox を扱う #321–#323 を土台に、親 agent の会話や生存に依存しない実行制御を daemon authority として実装する。

本 issue は supervisor の将来の正本である。実装済みとなるまで document/ の正本仕様には記載しない。

## 背景

#321–#323 は session upsert、agent 指定、即時 launch、run_id、caller↔worker binding、durable inbox、worker の完了／失敗報告を提供する。しかし「親が結果を受けて DAG の次 node を選び、再起動後も同じ判断を継続する」state machine、予算・並列・深さ・human escalation、成果物検証と最終完了判定は未実装である。

旧 #182/#183 の issue-DAG orchestration は session/PR 中心の別経路で完了済みであり、本 epic は agent run provenance と dispatch/inbox を使う daemon-owned supervisor を新設する。旧 state を暗黙に再利用・移行しない。

## 子 issue と依存 DAG

    #321 dispatch durable store ──┐
                                  ├── #325 durable supervisor run / task DAG / provenance store
    #322 dispatch runtime ────────┤      └── #326 event ingestion・wake/restart・scheduler loop
    #323 dispatch MCP/inbox ──────┘             └── #327 policy・cancel/escalation・verification/finalization
                                                      └── #328 MCP control/observation・compatibility migration・docs/test strategy

| issue | 担当 | dependson |
| --- | --- | --- |
| #325 | durable supervisor run、task DAG、親子 run provenance、状態遷移と event store | #321 |
| #326 | completion/failure/inbox event を受ける daemon scheduler、親 wake/restart、dispatch 実行 | #322, #325 |
| #327 | budget/concurrency/depth/retry/cancel/escalation policy、artifact verification gate、最終判定 | #323, #326 |
| #328 | supervisor MCP control/observation、既存 session_* と daemon Orchestrator の境界・後方互換性・移行、実装済み docs/test strategy | #323, #327 |

## Epic 受け入れ条件

- supervisor の durable state、外部 effect、通知、判断の責務が daemon に一意にあり、親 agent process が停止・再起動しても task DAG が重複実行せず再開する。
- すべての child run は parent supervisor run / task node / dispatch run / session / agent の provenance を durable に辿れる。
- completion、failure、NoReport、retry deadline、cancel、verification outcome は同じ event/reducer 経路で状態遷移し、少なくとも一回配送・再読によって収束する。
- policy が定める予算、並列数、深さ、retry と human escalation を越えて自律実行しない。
- task artifact の検証が通り、DAG の required node が terminal success になった場合だけ supervisor run を最終完了にできる。
- 既存 session_delegate_*、session_prompt、session_complete、#323 の dispatch/inbox API を破壊せず、用途と移行を明確にする。

## 非目標

- LLM に task 分解・次判断の正しさを保証させること。daemon は policy/fence/reducer を担当し、判断内容自体は agent input として扱う。
- TUI での専用 supervisor UX。
- 旧 #182 の session/PR-centric plan の自動移行。
- daemon crash 後の PTY FD 継続。document/proposals/07-pty-crash-continuation.md の範囲である。
