---
number: 185
title: feat(orchestration): TUI sync で DAG reconcile と worker dispatch を再開する
status: todo
priority: high
labels: [orchestration, tui]
dependson: [183, 184]
related: []
parent: 182
created_at: 2026-07-10T23:56:44.521611+00:00
updated_at: 2026-07-10T23:56:44.521611+00:00
---

## 背景

owner agent が終了した後も、常駐 daemon を追加せず既存の TUI sync/autostart から進行を再開する必要がある。

## やること

- TUI の既存 sync/autostart tick から durable reconcile entry point を呼ぶ。
- owner ended/不在かつ actionable な場合、集約 prompt を owner launch queue に冪等投入する。
- worker は owner 直下だけに作り、plan 上限と global agent 同時実行上限の小さい方を守る。
- `retry_wait`、`timeout`、`review_wait`、`merge_wait` を agent 枠を浪費せず遷移させる。

## 受け入れ条件

- TUI 停止中の状態/event が次回起動で収束する。
- concurrency 上限中は待ち、枠解放後に一度だけ delegate する。
- owner/worker 再起動と autostart failure で action を失わない。
- sub/subsub session を生成しない。
