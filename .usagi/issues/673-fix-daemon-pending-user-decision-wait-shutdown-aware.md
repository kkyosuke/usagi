---
number: 673
title: fix(daemon): pending user decision wait を切断・shutdown aware にする
status: done
priority: high
labels: [review, v2, daemon, mcp, decision, lifecycle, availability]
dependson: []
related: [329, 406, 557, 658, 689]
parent: 671
created_at: 2026-08-13T00:13:57.985707+00:00
updated_at: 2026-08-17T22:59:31.880272+00:00
---

## Finding（P1 availability / lifecycle）

`src/runtime/daemon.rs::wait_for_user_decision` は `user_decision_request` の同期応答を実現するため、25 ms ごとに `user-decisions.json` を読み直す無期限 loop で `Pending` の解決を待つ。`expires_at` は任意であり、loop は client disconnect、generation retirement、daemon shutdown を一切観測しない。

1 call が accepted socket と `usagi-ipc-client` worker を保持する。client が先に切断しても handler は socket IO に戻らないため worker は残る。shutdown / rollover は `ClientWorkers::retire` で全 worker を join するが、この worker は store polling を続けるため retirement が完了しない。期限なし request を繰り返すと bounded connection slots も枯渇する。

## `b928b74f` / #689 後も残る理由

#689 は、通常の frame read で park した client workerをretirement flag + bounded `poll(2)` readinessで起こし、`shutdown(2)` のwake-up取りこぼしに依存せずjoinできるようにした。これはsocket read中のworkerには有効である。

`wait_for_user_decision` はadmitted request handlerの内側でstore pollingしており、その間は`RetiringReader::read`へ戻らない。したがって#689のretirement flagを観測する機会がなく、client disconnect / daemon shutdown / generation retirementで待機を終了できない。本findingは`origin/main` `b928b74fd58b62a5cb73f3e1ace8c5c38188ace3`でも継続する。

## 修正方針

- pending waitをstore pollingではなくnotification / cancellation-awareなwait portにする。
- decision resolve / cancel / expireのstate transition、client disconnect、daemon / generation shutdownのいずれでもwaiterを起こす。
- disconnect / shutdownではworker / connectionだけを解放し、durable Pending recordを暗黙回答・削除しない。再接続後はget / listまたは同じidempotency keyで観測できる。
- pollingを残す場合もdisk read cadence、絶対deadline、shutdown cancellationを明示的にboundedにする。ただし固定25 ms sleepをworker数だけ増やさない。
- cancellation tokenはrequest handlerまで明示的に注入し、socket readerのpollへ偶然戻ることをtermination contractにしない。

## 受入条件

- [x] `expires_at`なしで回答待ち中のclientを切断すると、decisionはdurableに残り、client worker / connection slotはbounded time内に解放される。
- [x] 同じ状態でdaemon shutdown / generation retirementが全client workerをbounded time内にjoinできる。
- [x] resolve / cancel / expireは待機中の元callを一度だけ起こし、既存の同期回答契約を維持する。
- [x] N件pendingでもidle read / fsync / wakeup rateがN × 40/sにならない。
- [x] restart、duplicate idempotency key、late resolve、foreign ownerは既存のfail-closed契約を保つ。
- [x] #689の通常frame read retirement回帰testと、decision handler内でparkしたworkerのdisconnect / shutdown testを両方維持する。

## 根拠箇所

- `src/runtime/daemon.rs::wait_for_user_decision`
- `src/runtime/daemon.rs::start_ipc_accept_loop`
- `src/runtime/daemon.rs::RetiringReader`
- `crates/daemon/src/usecase/authority/workers.rs::ClientWorkers::retire`
