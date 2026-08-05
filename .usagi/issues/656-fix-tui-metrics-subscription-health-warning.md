---
number: 656
title: fix(tui): 未消費 metrics subscription による health warning の自己誘発を止める
status: todo
priority: high
labels: [review, stability, tui, daemon, metrics, diagnostics]
dependson: []
related: [297, 491, 645]
parent: 654
created_at: 2026-08-05T01:16:04.802098+00:00
updated_at: 2026-08-05T01:16:04.802098+00:00
---

## 症状

TUIを通常起動してdaemonが正常に応答しているだけでも、Home / daemon modal が `MetricsUpdatesDropped`（更新の取りこぼし）warningを表示し得る。

## 原因

production TUIは二つのmetrics経路を同時に作る。

1. `run_with_metrics_hook` がTUI lifetime用のconnectionで `MetricsAction::Subscribe` を送る。
2. Homeのresident metrics laneが1秒 cadenceで `MetricsAction::Snapshot` を送る。

Subscribeでdaemon側に作られた `MetricsObserver` の1-slot receiverはTUIから一度もdrainされない。各Snapshotは`MetricsBroker::publish`を呼ぶため、最初のpublishでqueueが埋まり、その後は毎回 `TrySendError::Full` となってbroker全体の `dropped_updates` を増やす。

health trackerは `dropped_updates >= 1/s` が3 sample連続するとwarningにするため、診断用observer自身が正常daemonを劣化扱いにする。

## 既存issueとの境界

- #297 はmetrics subscription導入契約。
- #491 はproduction `MetricsBroker` authority統合。
- #645 は`dropped_updates`を含むhealth indicator。

各issueは完了済みだが、「subscriberを実際にconsumeする経路」と「Snapshot pollingとの二重化」の組合せは固定していない。本issueはこのcomposition gapだけを扱う。

## 修正方針

次のどちらか一つをSSoTとして選び、二重観測を残さない。

- pollingを正とする: 未使用のSubscribe/Unsubscribe hookを削除し、Snapshot laneだけでmetrics/healthを更新する。
- pushを正とする: connection workerからmetrics eventをTUIへdrainし、resident Snapshot laneを置き換える。再接続時は再購読し、bounded coalescingを守る。

表示専用metricsのためにdaemonをcold-startしない既存契約は維持する。

## 受け入れ条件

- idleなTUIを複数metrics cadence動かしても、consumer自身を原因とする `dropped_updates` 増加が0である。
- 正常daemonを30秒以上観測しても `MetricsUpdatesDropped` warningが出ない。
- 意図的にdrainを停止した別slow subscriberではcounterとwarningが従来どおり増える。
- TUI disconnect/quit後にsubscriberが残らず、再接続時に重複登録しない。
- production compositionを通したfake/real IPC testで、Subscribe/Snapshot/Unsubscribeのcall countとbroker snapshotを固定する。
- metrics unavailable、daemon未起動、rollover/reconnect時のfreshness re-baselineを回帰させない。

## docs

`document/03-tui.md` と `document/05-daemon.md` でmetricsのtransport（pushまたはpoll）とslow-subscriber判定の正本を一つにする。
