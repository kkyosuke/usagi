---
number: 182
title: feat(orchestration): issue DAG 永続オーケストレータ
status: todo
priority: high
labels: [orchestration, epic]
dependson: []
related: [105]
created_at: 2026-07-10T23:56:40.131738+00:00
updated_at: 2026-07-10T23:56:40.131738+00:00
---

## 背景

単一の root/統括 session が issue DAG 全体を所有し、worker session を直接生成する永続オーケストレータを導入する。設計は [proposal](../../document/proposals/03-durable-issue-orchestrator.md) を正本とする。

## スコープ

- main 基準の issue ready を維持し、work-ready と merge-ready を分離する。
- durable state/event、終端通知、TUI reconcile、retry/timeout、stacked PR/CI policy を段階実装する。
- 専用 daemon と多段 sub-session を基本構造にしない。

## 受け入れ条件

- 子 issue #183–#187 が完了し、proposal の実装済み事項が正本ドキュメントへ反映される。
- 二重委譲、親停止、通知重複、PR 未 merge、retry/CI failure の統合テストが通る。
