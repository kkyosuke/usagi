---
number: 522
title: fix(tui): pane pending OperationIdをproduction launch ACKまで同一に保つ
status: todo
priority: high
labels: [review, v2, tui, agent, terminal, ipc, idempotency]
dependson: [518]
related: [215, 271, 506]
created_at: 2026-07-22T11:40:51.774650+00:00
updated_at: 2026-07-22T12:18:35.275636+00:00
---

## 問題・影響

TUI controllerはpane pending用のproducer `OperationId`（A）を発行するが、pane workerの `AgentCommandPort::launch` はoperationを受け取らず、production adapterが別の `OperationId`（B）を生成してdaemonへ送る。UIはAのcompletionとしてBのside effectをpromoteし得る。

adapterは `DaemonReply::Accepted.operation_id` をAと照合しない。さらにcompleted Agent launchは `ResponseOutcome::Ok` でterminal/continuation/relation/`completed: true`を返すが、`DaemonReply::Ok`自体にも現行bodyにもoperation identityがない。そのままではfinal/cached replayをAへ安全に相関できず、本issueのacceptanceが成立しない。

## #518 / #506との責務境界

#518 `refactor(daemon): owner-generation runtime shard と global resource allocator` が**generic Terminal Launch**のproducer OperationId、daemon wire/store、durable outcome replayを所有する。本issueはそこへ依存し、generic daemon contractを重複実装しない。

一方、既存durable Agent launchのcompleted/final responseをidentity-bearingにするadditive Agent server wire/presentation changeは本issueが所有・coordinateする。Agent store/idempotency semanticsを再設計せず、保存済みoperation identity + canonical semantic digestをfinal projectionへ露出する。

本issueは**同一TUI process内**のcontroller pending Aをworker/production adapter request/Accepted/final completionまで貫通させる。#506 active writerのtab intent/pending persistenceは変更しない。TUI reopen後のin-flight replay、OperationId復元・再利用は非対象で、#506契約に従いblind replayしない。

## Agent final wire contract

- Agent `Accepted` はenvelope `operation_id=A`を必須とし、request Aと照合する。
- Agent completed `Ok` とcached completed replayはbodyにcanonical `operation_id=A`、canonical semantic digest、`completed: true`、full TerminalRef/continuation/relationを必須で含める（またはprotocol全体をidentity-bearing final variantへadditive revisionする）。
- missing/invalid/mismatched operation ID、digest mismatch、`completed: false`をfinalとして受信、wrong target/ref/relationはsafe correlation failure。terminalをpending Aへpromoteしない。
- cached replayもdirect finalと同じidentity/digestを返し、adapterは経路によって検証を省略しない。

## 対象責務

- Agent/generic pane launch port requestにcontroller発行Aを必須化し、adapterで新規Bを生成しない。
- Agentは既存durable launchへAを渡し、本issueのidentity-bearing Accepted/Ok/cached final wireを検証する。genericは#518 contractへAを渡す。
- Accepted/final/cached replyのoperation ID、semantic digest/target、TerminalRef fenceを要求Aと照合する。
- 同一processでpending Aが存続する間だけlate/out-of-order completionをAへ相関し、closed/replaced tabを復活させない。
- timeout/ACK lossでは別operationを生成せずeffect unknown/pendingを安全に表示し、自動再送・reopen replayを導入しない。

## 受入条件

- [ ] 同一TUI processのcontroller→worker→adapter→daemon request→Accepted/Ok final→pending completionで同じAが観測され、adapter生成Bは存在しない。
- [ ] Agent direct `completed: true` Okとcached completed replayがidentity A + digestを返し、一致時だけ同じpending Aをcompleteする。
- [ ] Accepted mismatch、Ok finalのmissing/mismatched A、digest/wrong target/ref/relation、`completed: false`、late replyはpendingを成功化しない。
- [ ] concurrent root/session、Agent/generic、direct/cached、out-of-order completionでcross-talkがない。
- [ ] ACK loss後に別OperationIdでblind retryせず、TUI reopen後のreuse/replayを本issueが追加しない。
- [ ] generic wire/storeは#518だけを利用し、Agent final identity projectionだけを本issueが追加する。

## 必須回帰テスト

fixed Aを注入したcontroller/worker/production adapter/実IPC testで以下を検証する。

- Accepted A一致/不一致。
- Agent direct `Ok { completed: true, operation_id: A, digest }` とcached replayの一致。
- completed=trueだがmissing/mismatched operation ID、same A/different digest、wrong target/ref/relation。
- completed=falseをfinalへ誤投影しない。
- 2件out-of-order、pending close後late completion、Agent/generic並行cross-talk。
- request/Accepted/final/cachedのwire fixtureがAを一貫して持ち、spawn countやdurable Agent semanticsを変更しない。

reopen replay/spawn dedupeは#506/#518側contractとして本issueでは実装しない。

## docs

`document/03-tui.md` の同一process pending identity、`document/04-ipc.md` のAgent identity-bearing finalと#518 generic launch operationをSSoTとして更新し、#506 reopen intentから相互参照する。
