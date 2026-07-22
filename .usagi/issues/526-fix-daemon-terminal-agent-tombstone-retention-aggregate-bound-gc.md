---
number: 526
title: fix(daemon): terminal/Agent tombstone retentionをaggregate boundでGCする
status: todo
priority: high
labels: [review, v2, daemon, terminal, agent, resource, retention, gc]
dependson: [525]
related: [472, 473, 506, 509, 510, 518, 519]
created_at: 2026-07-22T11:43:25.850125+00:00
updated_at: 2026-07-22T11:56:37.831500+00:00
---

## 問題・影響

#472 は各terminalのraw replayを64KiBへbounded化し、#473 はexited PTY transport/FDを解放する。しかしregistry/durable storeに残るexited generic terminal、Agent runtime history、final replay/tombstone/journalの総数・総byte・ageにはworkspace/daemon-wide boundとGCがない。短命terminal/Agentを繰り返すと、per-entry boundを守ってもmemory/disk/index scan/IPC inventoryが無制限に増える。

unobserved finalを無期限保護すると、全件がunobservedなworkloadではhard aggregate capと両立しない。本issueは無期限保護を約束せず、minimum visibility TTL、soft reserve、pre-admission reservation、pressure時のtyped outcomeを一体で定義する。

## 既存issueとの境界

#472はper-stream pipeline bound、#473はlive transport/FD lifetime。本issueはeffect後の**terminal/Agent runtime record・final tombstone・replay/journal aggregate retention**だけを所有する。#525がfinal replayのobserved/dismissed到達性を提供するため、それに依存してminimum TTL中に観測可能にする。

#518が所有するlaunch operation outcome/relation/global allocator、#519が所有するinput sequence/ACK replay ledger・idempotency fenceは本issueの対象外であり、削除policyやstore schemaをここで定義しない。runtime recordからそれらowner管理recordへの参照が残る間はowner contractに従って保護し、各ownerのGCを重複実装しない。#506はAgent tab表示intent、#509/#510はprovider continuation/resume identityを所有する。

## retention / admission契約

- finalはobserved/unobservedにかかわらず設定されたminimum visibility TTLまでは保護する。TTL後のunobserved finalはpressure下でdeterministic eviction対象になり得て、無期限保護しない。
- soft reserve到達時にGCとlaunch admission backpressureを開始する。新規terminal/Agent launchはworst-case final tombstone/replay budgetを事前予約し、hard cap内にreserveできなければspawn前にtyped `ResourceExhausted` で拒否する。
- admission済みlive runtimeのexitは予約済みcapacityへfinalを保存し、hard capを理由にexit結果をsilent dropしない。
- hard cap evictionはminimum TTL経過済みclassだけを対象とし、query/inventoryにはtyped retention-expired/evicted outcomeを返す。missingや別historyへのfallbackにしない。
- migration/corruption等で予約を超過する緊急時のcompact typed eviction markerとoperator-visible metricをboundedに定義し、silent deletionを禁止する。

## 対象責務

- exited generic terminal、completed/interrupted Agent runtime、final replay/tombstone/journalを分類し、daemon/workspace/userごとのhard count/byte/age budget、minimum TTL、soft reserveを定義する。
- active/draining owned terminal、minimum-TTL final、eligible provider resume source、#506 dismissal/reopenに必要なruntime lineageを誤GCしない。
- observed/dismissed/superseded/expiredおよびTTL経過unobserved runtime finalの優先順とdeterministic evictionを定義し、atomic store/index/replay削除へ収束させる。
- GC後のruntime/final queryはtyped expired/evictedとなり、別historyへfallbackまたはruntimeを復活させない。
- startup/reconcile/periodic/event-driven GCをbounded workで実行し、partial failure/crash後にruntime index/replay/reservationを不整合にしない。
- retained/evicted runtime count/bytes/oldest age、reserve pressure、admission rejectionをsafe metricsで観測し、terminal output/provider IDをlogしない。

## 受入条件

- [ ] 全件unobservedを含む10万件相当の短命runtime workloadでもhard count/byte capを超えず、soft reserveでadmission backpressureする。
- [ ] minimum TTL中のfinalとadmission済みruntimeのreserved final capacityは保持される。
- [ ] TTL経過後はunobservedを含むeligible classがdeterministic順でevictされ、queryがtyped evicted outcomeになる。
- [ ] reserve不能なlaunchはspawn前にtyped rejectionとなり、既存finalのsilent deletionでcapacityを作らない。
- [ ] crash/failpoint後にdangling runtime index、orphan replay、leaked/double reservationを残さずretryで収束する。
- [ ] #518 operation outcome/relationと#519 input ledgerを削除・再定義せず、owner管理参照を破壊しない。
- [ ] #525 UIでminimum TTL中に最終結果を観測・dismissできる。

## 必須回帰テスト

small budget/short TTL fixtureでgeneric/Agent、live/exited/interrupted/resumed、全件unobserved、observed/dismissed、old/new、same-size tieを混在させ、reservation/admission rejection/eviction order/typed outcomeを固定する。store failpoint/crash/restart、concurrent exit/GC/admission、large-cardinality benchmark、metricsを検証し、#518/#519 owner recordが不変であることも確認する。

## docs / migration

`document/05-daemon.md` をruntime/final hard cap・minimum TTL・soft reserve・reservation・GC/typed expiryのSSoTとし、`document/03-tui.md` のhistory dismissalから参照する。#518/#519のoperation/input retentionは各owner文書を参照し、本issueへ取り込まない。既存unbounded runtime recordsのincremental migrationとover-cap時のbackpressureを定義する。
