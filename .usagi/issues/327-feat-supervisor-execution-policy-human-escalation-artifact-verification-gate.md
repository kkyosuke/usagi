---
number: 327
title: feat(supervisor): execution policy・human escalation・artifact verification gate を実装する
status: done
priority: high
labels: [orchestration, supervisor, policy, verification, daemon]
dependson: [323, 326]
related: [183, 187, 219, 283, 329, 330]
parent: 324
created_at: 2026-07-17T21:12:25.962916+00:00
updated_at: 2026-07-18T00:25:33.721530+00:00
---

## 目的

durable supervisor が無制限に再帰・再試行・委譲せず、安全な execution policy に従って進行・cancel・human escalation・artifact verification・最終完了判定を行うようにする。

## policy 契約

SupervisorRun 作成時に immutable policy snapshot/revision を保存する。最低限、run 全体と task subtree ごとに次を持つ。

| policy | 強制すること |
| --- | --- |
| execution budget | dispatch 回数、active time、token/cost が取得可能な場合の上限。計測不能値を成功扱いしない |
| max concurrency | supervisor run と parent task ごとの in-flight worker 上限 |
| max depth | task DAG の parent→child 深さ上限 |
| retry | retryable category、最大 attempt、exponential backoff/jitter、次試行時刻 |
| cancellation | run/task cancel の authority、in-flight dispatch の fenced cancel、terminal convergence |
| escalation | budget/depth/retry 超過、ambiguous provenance、verification failure、policy が判断不能とする条件を Escalated にする |
| verification | task artifact の required evidence、verifier kind、timeout、success/failure/indeterminate の扱い |

初期値／workspace 設定の導入は実装時に一つの正本を決める。client が request ごとに緩い上限へ上書きすることは許可しない。

## やること

- #325 reducer/#326 scheduler の action admission 前に policy evaluator を接続し、budget/concurrency/depth/retry/cancel/escalation を deterministic に判定する。reservation と counter を durable state に同時 commit し、restart/duplicate event で上限を超えないようにする。
- failure/NoReport/timeout を typed category に正規化する。retry 可能な failure だけを scheduled retry にし、attempt を消費する。retry deadline 到達は scheduler event として再現可能にする。
- cancel を SupervisorRun と TaskNode の状態遷移として実装し、新規 dispatch/wake/retry を止める。すでに起動済み worker は #322 の operation/session fence を通じて cancel し、ACK loss・late completion でも cancelled run を success に戻さない。
- human escalation record を durable に保存する。理由、blocking task/run、必要な選択肢、安全に表示可能な evidence、resume/cancel decision を持ち、agent が自律的に escalation を解除しない。
- agent が明示的に要求する user decision の durable payload と non-blocking MCP 契約、TUI 操作は
  #329 / #330 に分離する。本 issue は policy が escalation に入る条件と、解決前に新しい effect を出さない
  admission rule を所有する。
- artifact verification gate を追加する。worker の StructuredResult は自己申告であり、required verifier（例: PR merged、commit/worktree fence、指定 command の non-secret result、review approval を実装時に選択）を通過するまで task/parent/run を Succeeded にしない。verifier は effect reservation・timeout・result digest を durable に保存する。
- DAG の required task が verification 済み Succeeded、optional task が明示 policy に従い terminal、in-flight effect が無い時だけ SupervisorRun を Succeeded にする。verification failure/indeterminate は retry policy または Escalated/Failed に収束させる。
- pure evaluator、fake verifier/clock、daemon integration で境界値を検証する。

## 受け入れ条件

- concurrency/depth/budget/retry 上限の競合 request、restart、duplicate completion で上限を一度も超えず、超過は durable な Escalated/Blocked の理由になる。
- cancel は in-flight/queued/retrying task を一意に収束させ、late success/failure が terminal cancelled state を覆さない。
- retryable/non-retryable/no-report/timeout が指定の attempt/backoff に従い、次試行時刻前に再起動しない。
- required artifact の独立 verification が成功するまで task/parent/supervisor run は最終 success にならない。worker の summary/PR URL 単独では gate を通らない。
- human escalation は daemon restart 後も読め、明示的な authorized decision があるまで scheduler が新しい effect を出さない。
- policy/reducer/verifier/integration tests を追加し coverage 100% を維持する。

## 非目標

- 人間が選択する UI（#328 の API までは含むが TUI は別 issue）。
- 任意の CI/forge provider を全て自動連携すること。初期 verifier の対象は実装時に最小かつテスト可能な契約に限定する。
