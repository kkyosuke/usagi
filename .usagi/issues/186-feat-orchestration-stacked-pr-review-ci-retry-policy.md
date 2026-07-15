---
number: 186
title: feat(orchestration): stacked PR・review・CI retry policy を安全に統合する
status: done
priority: high
labels: [orchestration, github]
dependson: [183, 185]
related: []
parent: 182
created_at: 2026-07-10T23:56:45.888900+00:00
updated_at: 2026-07-10T23:56:45.888900+00:00
---

## 背景

先行 PR の merge 前に後続作業を進めつつ、review 順序、base drift、CI failure、merge 可否を明示的に扱う必要がある。

## やること

- work-ready の base ref と依存 PR/head commit を記録する。
- PR base は main のまま、本文の `Depends-on`、review/merge 順序、rebase 手順を生成・検証する。
- 直列 chain の先行 head 基点だけを自動許可し、join、競合、head drift は escalation にする。
- `review_wait` / `ci_wait` / `ci_failed` / `retry_wait` と attempt 上限、指数 backoff + jitter、failure fingerprint を実装する。
- 同一 CI failure の修正 session 再投入を制限する。

## 受け入れ条件

- 先行 PR 未 merge でも後続は work-ready になり得るが merge-ready にはならない。
- main base 強制と branch protection を回避しない。
- retry 上限、同一 failure 反復、timeout、conflict が人への escalation へ収束する。
- 依存表示欠落、base drift、join node を自動 merge しないテストがある。
