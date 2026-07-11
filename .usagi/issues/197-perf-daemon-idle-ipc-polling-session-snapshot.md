---
number: 197
title: perf(daemon): idle IPC polling と session snapshot 再読込を削減する
status: done
priority: medium
labels: [perf, daemon, ipc, review]
dependson: []
related: [161, 163, 167]
parent: 159
created_at: 2026-07-11T01:30:36.160437+00:00
updated_at: 2026-07-11T06:20:20.119195+00:00
---

## 背景

daemonはclientが1つでも接続中なら15ms周期でpollする。現在の各pollはrequestやoutputが無くても次を行う。

- `sessions.json` をfilesystemからread・JSON parse
- client idの `Vec` allocate
- 全terminal idをcollect/sortしてoutput確認
- 全terminal idを再度collect/sortしてexit確認

したがって通常のremote paneが1つあるだけで、session snapshot readが毎秒約66.67回発生する。background paneを含む各 `DaemonTerminal` はconnectionを保持するため、idle workspaceでもfast cadenceが継続する。

## 方針

- Unix socketとPTY outputを `poll/kqueue/mio` 等のready eventで駆動する。
- daemon monitorが生成したsession snapshotをmemory cacheし、`ListSessions` / `Subscribe` 時にfilesystemを再読込しない。
- outputはdirty terminal setから配信し、terminal livenessは低頻度cadenceまたはexit signalへ分離する。
- stable iterationが不要なhot pathではterminal idのcollect/sortを避ける。
- fast cadenceが必要なinput latencyと、slow control-plane処理を別loop/workerへ分離する。

## 受け入れ条件

- client接続済み・request/outputなしのidle期間にsessions storeを再読込しない。
- idle CPU/wakeupがterminal数に比例して増えない。
- input echo latencyとoutput delivery latencyが現行水準を悪化させない。
- disconnect、terminal exit、session transition通知を取りこぼさない。

## テスト・計測

- fake store counterでN回idle pollのread回数0を保証。
- 1/10/100 terminal、1/10 clientのidle CPU/wakeup benchmark。
- burst input/output、slow client、disconnectのE2E。
- before/afterのlatency percentilesを記録する。
