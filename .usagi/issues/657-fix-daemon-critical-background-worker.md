---
number: 657
title: fix(daemon): critical background worker の停止を監視し部分故障を残さない
status: todo
priority: high
labels: [review, stability, daemon, resilience, worker]
dependson: []
related: [515, 645]
parent: 654
created_at: 2026-08-05T01:16:04.895239+00:00
updated_at: 2026-08-05T01:16:04.895239+00:00
---

## 問題

daemonはprocess-wide panic hookでworker panicをerror logへ記録するが、IPC accept worker以外の多くのbackground workerは`JoinHandle`を保持・監視していない。workerがpanicまたは予期せずreturnしてもdaemon PID、record、socketは生存し、`usagi daemon status`はrunningを返す。

対象例。

- Agent PTY observer
- generic terminal observer
- connection cleanup
- session teardown
- PR projection / refresh
- decision maintenance
- retention GC
- draining collection / custody supervision

部分故障時には、terminal output/exitだけ更新されない、sessionが`deleting`のままになる、PR検出やdecision expiryだけ停止する等、表面ごとに異なるstuck stateが残り得る。

## 既存issueとの境界

#515 はIPC accept workerのunexpected exitをshared shutdownへ接続済みである。本issueはそれ以外のdaemon-owned workerのlifecycle/supervisionを同じ安全水準へ揃える。

## 修正方針

- daemon compositionが全workerのhandle、役割、終了理由を一つの`WorkerGroup`（名称は任意）で所有する。
- workerを明示的に分類する。
  - critical: 消失後に安全なdaemon serviceを継続できない。unexpected exitでadmissionを閉じ、daemon全体をgraceful shutdownする。
  - restartable: bounded backoff/attempt上限の下で再起動できる。
  - optional/degraded: service継続は可能だがtyped healthへ劣化を公開する。
- shutdown時は全workerへ停止を通知し、join結果を集約してendpoint/record cleanup前後の順序を固定する。
- worker panic/poisonを単なる空一覧や無言の`break`として永続化しない。raw payload/PTY outputをhealthへ出さない。

## 受け入れ条件

- 各workerにinjected panic / early return / channel closeを発生させ、分類どおりshutdown・restart・degradedへ収束する。
- critical worker停止後、PIDだけ生きた`daemon running`状態を残さず、clientはdeadline内に切断またはtyped unavailableを受ける。
- session teardown worker停止で`Deleting`を永久放置しない。再起動またはdaemon restart後のdurable drainへ収束する。
- Agent/generic observer停止後に新規input/outputを成功扱いしない。
- orderly shutdownでworker、socket、PTY transport、lock/record/locator cleanupがjoin順序を守る。
- worker状態またはfailure reasonをdaemon status/metricsへ公開する場合、閉じた安全語彙だけを使う。
- real daemon subprocessのfault-injection testを少なくともcritical worker一種で追加する。

## docs

`document/05-daemon.md` にworker class、unexpected-exit policy、shutdown/join order、operatorが見るhealth/logを記載する。
