---
number: 518
title: refactor(daemon): owner-generation runtime shard と global resource allocator を実装する
status: todo
priority: high
labels: [review, v2, daemon, runtime, terminal, generation, durability, recovery]
dependson: [516]
related: [209, 221, 255, 350, 459, 474, 492, 508, 514, 515, 526, 528]
parent: 507
created_at: 2026-07-22T11:37:07.035271+00:00
updated_at: 2026-07-22T12:07:38.933057+00:00
---

## 問題・根拠

#507 の planned rollover では旧 draining generation が terminal exit を処理する一方、新 active generation が新規 Agent / generic Terminal を spawn する。現状の runtime 永続化と capacity authority は複数 process の同時進行を安全に表現できない。

- Agent は `agents.json`、generic Terminal は `terminals.json` を process ごとの memory stateから whole snapshot で atomic replace する。cross-process read-modify-write lock / revision CAS / merge はなく、G1 の exit と G2 の spawn が同じ古い snapshot から書くと last rename が一方を失わせる。
- `GenerationSnapshot` は Agent snapshot 内だけにあり、generic Terminal record と共通の resource owner authority ではない。
- concurrency limit / capacity reservation は各 process の memory 内にあり、active と draining の合計上限、予約、release を一意に管理できない。
- generic `TerminalRequest::Launch` は producer `OperationId` を wire に持たない。server は request ごとに `TerminalId` と `OperationId::new()` を発行し、durable record は terminal ID でのみ索引する。spawn 後の response / ACK loss で client が再接続して launch を再送すると、同じ intent を識別できず二重 spawn し得る。connection-local `RequestId` は durable idempotency key ではない。
- production Agent / generic PTY の `ProcessIdentity.start_identity` は固定文字列で、PID reuse と別 incarnation を区別できない。planned rollover で「この child はこの owner generation が起動した同じ process」と証明できず、exit/kill/reconcile authority に使えない。
- #526 は runtime / final tombstone retention に限定され、global allocator の completed claim と Agent / generic launch operation outcome ledger は対象外である。本 issue が独自の retention / expiry / GC contract を持たなければ、repeated launch により別の unbounded growth が残る。

endpoint/generation role の authority は #516、daemon owner process の exact identity と shutdown primitive は #514、locator publish の crash recovery は #515 が担当する。本 issue はそれらを再実装せず、resource state・allocation・child identity の cross-process contractを担当する。

## 対象責務

owner generation ごとの single-writer shard と、全 generation が共有する global resource allocator / event handoff を実装する。

1. Agent と generic Terminal の durable runtime record を owner `DaemonGeneration` ごとの shard に分離する。各 daemon process は自 shard だけを書き、別 generation の whole snapshot を置換しない。
2. global allocator は resource ID、owner generation、resource kind、capacity pool、producer operation ID、semantic digest、capacity claim、state/revision を CAS で管理する。policy は現行の Agent / generic Terminal 別上限を暗黙に合算せず、pool/kindごとの上限を全active/draining processで一意に予約・解放する。
3. TUI `Effect::OpenTerminal` / `OpenTerminalRequest` が既に持つ canonical producer `OperationId` を backend、wire、daemon、allocatorまで脱落させず end-to-end で保持する。同じ ID + 同じ canonical intent は予約済み/final outcome と同じ `TerminalRef` を replayし、異なる intent は idempotency conflict として effect zero で拒否する。global claimをauthorityとして先に保存し、owner shard reservationもdurableになった後だけ一度spawnする。片側だけ残ったclaim/reservationはleaked/ownership unknownとしてfail closedにし、推測spawn/releaseしない。Accepted responseはproducer IDとdurable revisionをそのまま返し、successだけでなくdefinite failure、ambiguous spawn、persist-after-spawnもdurable finalとして再送する。
4. Agent と generic child spawn の `ProcessIdentity` に OS が検証できる process-start identity と process-group identity を記録する。固定文字列、wall clock、PID-only fallback は authority にしない。#514 の process identity primitiveを再利用できる場合は共通化するが、daemon lifecycle signal 契約は変更しない。
5. draining owner は terminal output/exit/command completionとoutboxを自 shardへ一度だけcommitし、global consume ledgerへterminal identity、owner generation、event revisionを発行する。active consumerはold shardへ直接ACK writeせず、owner single writerがglobal consumed revisionを観測して自outboxを回収する。旧PTY observationから共有`pr-inventory.json`をwhole-saveする経路や旧processのsupervisor tickなど、draining processが触れ得る他のshared writerもinventory化し、owner-local eventをactive single writerがconsumeするか同等のgeneration fenceを設ける。Agent/Terminal shardだけ直して他のshared snapshotにlost updateを移さない。
6. active generation は exit event を idempotently consumeし、global capacity と active projectionを一度だけ更新する。ACK loss、consumer restart、duplicate/late/wrong-owner event は同じ outcomeへ収束し、別 resourceを変更しない。
7. standbyはowner shardをread-onlyでhydrateし、#516のreadiness中にreconcile/save、worker/tick、spawnを行わない。handoff commit後にだけ自 shardをactive writerとして開き、sealed hydrate revisionとglobal allocator revisionを再検証してadmissionを開始する。
8. old generation の collection eligibility は、自 shardの live resource 0、in-flight terminal command 0、未 ACK outbox 0、global capacity claim 0 をすべて検証して決める。registry role/endpointの最終回収は #516 / 親 #507 が行う。
9. legacy `agents.json` / `terminals.json` は完全な `TerminalRef.daemon_generation` と検証可能な child identityが一致する recordだけを owner shardへ移す。ambiguous、unknown/corrupt schema、固定/欠損 identityは ownership unknownとして fail closedにし、推測 spawn・kill・capacity releaseを行わない。旧activeがreal-child-identityとsharded-store capabilityをadvertiseする場合だけplanned rolloverを許可し、非対応versionからのbuild replacementはseamless継続を偽らずrefuseまたは明示cold transitionを要求する。
10. global allocator の completed claim と Agent / generic launch operation outcome / final ledger に、count、serialized byte、age の hard limit と documented minimum idempotency window / expiry horizon を持つ bounded retention / expiry / GC を実装する。minimum window 内は同じ `OperationId` と semantic digestへ full exact outcomeをreplayする。window 経過後に full outcomeを削除するときは、`OperationId`、semantic digest、expiry class/cutoffを持つ compact non-reusable tombstoneへatomicに置換し、expiry horizon中の再送をtyped `operation_expired`としてeffect zeroにする。exact tombstoneをGCできるのは、serverが受理済みhistoryから進めるdurable monotonic UUIDv7 expiry watermarkにより、そのID以下を永久にfresh admissionしない場合だけとする。evicted / too-old ID、retained tombstone、watermark以下のIDはunknownな新規launchとして再予約・capacity claim・spawnせずtyped `operation_expired`へ収束する。incoming future timestampだけでwatermarkを進めない。GC対象はdurable finalとexact-once capacity release revisionがcommit済みで、全outbox/consumer ACKとdependency 0を確認したcompleted recordだけとする。live claim、owner reservation、unacked outbox、in-flight / ambiguous recordはageやhard capに達してもGC対象外である。minimum window / expiry horizonを破らず安全にGCできないままcount/byte hard capへ達した場合は、既存recordをevictせず新規Agent/generic launchをtyped backpressureでeffect zeroに拒否する。このallocator/operation-ledger contractは#526へ委譲しない。

## 非対象

- cross-process generation role、standby endpoint、locator handoff、request admission fence（#516）
- shipping `daemon restart` / build replacement / stop の orchestration（親 #507）
- draining endpoint を選択する client routing と multi-generation inventory merge（#508）
- daemon owner PIDへの exact shutdown signal（#514）
- crash/SIGKILL 後の PTY fd回収（#221）
- runtime record / terminal final tombstone 自体の retention（#526）。ただし global allocator claim と launch operation outcome ledger の retention / expiry / GC は本 issue の対象に残す。

## 受入条件

- [ ] G1 exit と G2 spawn を同時実行しても、owner shardとglobal allocatorの両方に各 transitionが一度ずつ残り、lost updateがない。
- [ ] Agent / generic Terminal の各capacity poolは全retained generationで設定上限を超えず、現行のpool別policyを暗黙に合算しない。reservation failureはspawn effect zeroになる。
- [ ] global claim → owner reservation → spawn の各crash pointで片側だけのstateを推測解放/再spawnせず、同じoperationのsafe outcomeへ収束する。
- [ ] generic Terminal Launchのresponse/ACK loss、disconnect、same-process concurrent duplicate、restart/hydrate、daemon handoffは同じproducer operationと`TerminalRef`へ収束し、spawn countは1のままである。Acceptedは同じproducer ID/revisionを返し、definite/ambiguous/persist-after-spawn outcomeも同じfinalへ収束する。
- [ ] 同じ`OperationId`の異なるscope/profile/geometryはidempotency conflictとなり、既存terminalもcapacityも変更しない。
- [ ] Agentとgeneric childのstart identityはOS観測でexact match / gone / unknownを区別し、PID reuse・固定文字列・観測failureで別processをownerと認定、signal、releaseしない。
- [ ] draining ownerのexitとactive consumerのACKがlost/duplicate/reorderedになっても、terminal final、capacity release、projection更新はそれぞれ一度だけである。activeはold shardへ書かない。
- [ ] standby hydrate/readinessは全storeとworkerに対してread-onlyで、activation後だけsealed revisionからwriter/admissionを開始する。
- [ ] PR inventory、supervisorその他のdraining writerを含む全shared stateにsingle-writerまたはgeneration fenceがあり、G1 eventとG2 refresh/tickのlost updateがない。
- [ ] stale/wrong-generation event、別ownerのterminal command、corrupt shard/global ledgerはfail closedで、別resourceや新active snapshotを変更しない。
- [ ] old generationはresource、in-flight command、outbox、capacity claimがすべて0になる前にcollectableにならない。
- [ ] completed allocator claim と Agent / generic launch outcome ledger は configured count / serialized byte / age limit 内に収まり、minimum idempotency window 内の同一 operation は restart / handoff 後も full exact final を replayする。
- [ ] full outcome eviction は compact non-reusable tombstoneへのatomic replacementを経由する。expiry horizon後にexact tombstoneを削除してもdurable watermark以下のevicted / too-old `OperationId` はtyped `operation_expired`となり、fresh admission、reservation、capacity claim、spawnを再実行しない。GC と同時の retry / ACK lossでも replay safetyを保つ。
- [ ] final commit、capacity release、full-outcome → tombstone、expiry watermark、tombstone compaction の各 crash pointでcapacityは一度だけ解放される。live claim、owner reservation、unacked outbox、in-flight / ambiguous record、window内finalはGCされず、安全なGC候補なしでhard capへ達した場合は新規launchをtyped backpressureでeffect zeroに拒否する。
- [ ] legacy migrationはAgent/generic Terminalを同じowner-generation contractへ移し、証明不能recordをlive扱いしない。capability未対応のold activeはplanned rolloverを開始せず、安全なrefusalまたは明示cold transitionになる。

## 必須テスト

- two-writer interleavingでG1 exit / G2 Agent spawn / G2 generic spawnをbarrier同期するlost-update回帰
- global allocatorのpool別CAS/capacity、operation semantic conflict、global claim → owner reservation → spawnの全crash point
- generic Terminal Launchのresponse write failure / ACK loss / reconnect / restart hydrate / same-process concurrent duplicate replayでspawn count 1、同じproducer ID/revision/final
- Agent / generic childの実Unix process start identity、PID reuse相当、gone / permission denied / malformed observation
- draining outboxのduplicate、reorder、consumer crash、ACK loss、active restart、late old event、activeがold shardへwriteしない証明
- standby read-only hydrateとactivation revision seal、PR inventory / supervisorを含むshared-writer inventoryのG1/G2 barrier interleaving
- capability対応old activeのrollover許可と非対応old activeのsafe refusal / explicit cold transition
- shard schema corruption、partial migration、unknown generation、generation collection barrier
- 小さい count / byte / age limit と fake clock を使い、repeated Agent / generic launch、minimum window直前/直後、full outcome → compact tombstone → expiry watermarkの各phase、restart/handoffを進めてもstore sizeがboundedになるretention/expiry test。full outcome eviction後とexact tombstone compaction後にold `OperationId`を再送し、typed `operation_expired`、effect zero、spawn count不変を検証する
- final commit → capacity release → full-outcome/tombstone置換 → expiry watermark → tombstone compaction の各境界でcrash/restartするmatrix。live claim / owner reservation / unacked outbox / in-flight / ambiguous recordだけでhard capを満たす場合もGC/evictionせず、typed backpressureでfresh admissionを拒否することを検証する
- 2 daemon process・実PTY childを使い、old exitとnew spawnを同時に発生させるintegration test

## 依存関係

```text
#514 / #515 / #528 -> #516 generation registry/admission
                              |
                              v
               本 issue (owner shards / allocator / handoff)
                              |
                              v
                  #508 client owner routing
                              |
                              v
                  #507 shipping lifecycle / final E2E
```

## docs / gate

実装時は現在形の仕様を実装に合わせて更新し、特に次の既知誤記を解消する。

- [architecture](../../document/02-architecture.md) の generic launch response-loss が replacement spawn を防ぐという記述
- [IPC](../../document/04-ipc.md) の production `RequestId` response cache と mutation retry identity の一般化
- generic Terminal Launch の producer `OperationId`、Accepted/final replay、child process-start identity

本 issue が未実装の間は将来契約を仕様本文へ実装済みとして書かず、現行の ACK-loss / fixed identity 制約と本 issueへのlinkだけを記載する。Rust、wire schema、durable migration、process/PTY、cross-process writerに影響するため、fmt/check/clippy、selected/full tests、coverage 100%、Markdown link checkを必須とする。
