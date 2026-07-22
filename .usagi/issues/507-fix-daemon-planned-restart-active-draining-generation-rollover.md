---
number: 507
title: fix(daemon): planned restart を active/draining generation rollover に接続する
status: todo
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: [508]
related: [209, 221, 275, 350, 492, 514, 515, 528]
parent: 505
created_at: 2026-07-21T21:20:49.574125+00:00
updated_at: 2026-07-22T12:07:22.406618+00:00
---

## 問題・影響

shipping `usagi daemon restart` は [restart usecase](../../crates/daemon/src/usecase/restart.rs) から stop → fresh start を行う。旧 daemon process を終了するため、旧 process が所有する PTY master と live Agent / generic Terminal を draining generation として維持できない。fresh daemon の起動時 reconcile は unfinished runtime を `identity_unknown` に落とし、旧 `TerminalRef` を live として復元しない。

planned restart の state machine と owner fence は部分的に存在するが、2 process を同時に安全運用する production authority がない。このまま shipping restart だけを rollover に切り替えると、二重 active、late spawn、snapshot lost update、誤った owner/capacity release を起こし得る。

## shipping 実装の実証

最新 main の production path は次のとおりである。

- `serve` は process lifetime の `daemon.lock` を保持するため、旧 owner が生存中に standby daemon を同じ data directory で起動できない。
- `SecureUnixListener::bind` は generation endpoint の bind と `current.json` publish を一つの処理で行い、非公開 standby endpoint の readiness と active authority commit を分離できない。
- `restart::restart` は `stop::stop` 完了後に `start::launch_and_confirm` を呼ぶ cold replacement である。
- `GenerationCoordinator::rollover` は shipping lifecycle から呼ばれず、snapshot は Agent runtime 内の process-local authority に留まる。generic Terminal は同じ cross-process generation authority に含まれない。
- client / server `BuildIdentity` はともに `commit = "unknown"` で、bootstrap はversion/targetを含む完全一致をsame buildとする。productionは`force_restart = false`のため、same-version rebuildをold daemonと誤認してbuild rollover trigger自体を発行しない。このgapは[#528](./528-fix-daemon-build-artifact-identity-safe-rollover-trigger.md)がartifact identity / mode policy / safe triggerとして閉じる。
- current clientはactive locatorだけを中心に接続し、draining owner endpointへ`TerminalRef.daemon_generation`でrouteしない。shipping rolloverを先に有効化するとold PTYが生存しても到達不能・uncollectableになるため、#508をshipping enable前のprerequisiteへ前倒しする。
- shutdown shared flag を見るのは accept loop だけで、accept 済み client worker の JoinHandle は保持されない。既接続 handler は role/admission を再検証せず frame dispatch を継続できるため、accept 停止後にも reserve/spawn/control effect を開始できる。この race は [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) が request/internal-producer admission lease と worker drain で閉じる。
- `agents.json` / `terminals.json` は process memory から whole snapshot を atomic replace する single-writer store で、cross-process CAS/merge を持たない。G1 exit と G2 spawn の同時 write は last rename で一方を失わせる。generic Terminal Launch は producer `OperationId` を wire に持たず、Agent / generic child の `ProcessIdentity.start_identity` は production で固定文字列である。これらは [#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) が owner shard / allocator / event handoff と一緒に閉じる。

## #209 done と production gap

#209 は `active / draining`、effect 前後の generation fence、process-start identity、normal stop refusal、実 PTY E2E を受入条件にして `done` になった。しかし現在の shipping binary には上記の `daemon.lock`、即時 current publish、stop → fresh start、process-local snapshot、accept 済み connection race が残る。したがって #209 の完了は pure coordinator と当時の統合範囲の完了を示すだけで、planned restart の product-level 受入を実証しない。

本 issue を shipping rollover の canonical integration issue とし、#209 の要件を重複起票せず、未配線の production lifecycle と product E2E を完了させる。

## 依存分割と責務境界

```text
#514 daemon owner identity ─┐
#515 locator recovery ──────┼─> #516 generation registry / standby / admission
#528 artifact identity ─────┘                         |
                                                      v
                                  #518 owner shards / allocator / event handoff
                                                      |
                                                      v
                                  #508 client owner-generation routing
                                                      |
                                                      v
                                  #507 shipping restart / stop / final E2E
```

- #514 は daemon owner process の exact identity と shutdown primitive を担当する。terminal child identity は担当しない。
- #515 は locator temporary / secure atomic publish / crash recovery を担当する。generation CAS と handoff ordering は担当しない。
- [#528](./528-fix-daemon-build-artifact-identity-safe-rollover-trigger.md) はcanonical artifact identity、release/development policy、old daemonを止めないsafe rollover triggerを担当する。
- [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) は cross-process role/CAS、private standby readiness、locator handoff、既接続 request と internal producer の admission lease を担当する。
- [#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) は owner-generation shard、capacity pool、exit/outbox handoff、generic Terminal Launch idempotency、terminal child の実 process identity を担当する。
- [#508](./508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) は active/draining inventory merge と `TerminalRef.daemon_generation` による client routingをfixture上で先行完成させ、shipping enable capabilityを提供する。

#507 は #508 の完了まで `todo` に保ち、上記 primitive を再実装しない。

## 対象責務

依存 issue の完了後、planned restart を次の shipping lifecycle へ接続する。#508の`owner-generation-routing.v1` capabilityとcompatible registry revisionを全handoff participantで確認できない限り、shipping rollover pathはdisabledのままにし、old active/currentへeffectを与えない。

1. manual `usagi daemon restart` と build/update replacement を同じ durable rollover operation へ接続する。
2. old active へ rollover を予約し、side-effect-free standby start/readiness 後に #516 の authority handoff を commit する。authority commit前のfailureではold activeを維持し、一度observableになったcommit後はrollbackせず同じoperationをroll-forward / repairまたはfail closedへ収束させる。半端なcurrentや二重activeを残さない。
3. handoff 後の新規 Agent / generic Terminal は new active が #518 allocator から所有し、old draining は exact owner terminal operation と exit/outbox だけを継続する。
4. old generation は owned resource、active/terminal lease、outbox、capacity claim がすべて 0 になった後だけ endpoint/process/registry entry を回収する。
5. capability 非対応の old daemon から seamless build rollover を偽らず、安全な refusal または利用者が明示した cold transition にする。
6. `daemon stop` は rollover と別契約にする。live resource がある normal stop は拒否し、force-cold / terminate は terminal termination または interrupted reconcile の結果を確認してから停止する。後続 `daemon start` は cold resume 経路であり planned rollover ではない。

## 非対象

daemon crash / SIGKILL / OS reboot 後に旧 PTY master fd を回収することは対象外とする。broker / Unix FD handoff は #221、provider-native conversation resume は #503〜#510 の契約に従う。planned restart では provider CLI を再起動せず、同じ PTY / child process を旧 draining owner が継続する。

## 受入条件

- [ ] #508 のowner-generation routing capability / registry revisionを確認できないclient、旧build、partial deploymentではshipping rolloverを開始せず、old active/currentとlive PTYを維持したtyped refusalになる。到達不能なdraining resourceを生成できるintermediate mainを許さない。
- [ ] shipping `usagi daemon restart` は new active の readiness 後に authority を handoff し、live terminal を持つ old daemon を draining として残す。
- [ ] handoff後もTUI close/reopen、client reconnect、active locator切替をまたいでold `TerminalRef`は#508 routeでowner endpointへ到達でき、新規control/launchだけがactiveへ送られる。
- [ ] manual restart と build/update replacement は同じ tested rollover path を使い、通常経路に stop → fresh start の bypass を残さない。
- [ ] start/hydrate/bind/readinessとauthority commit前のfailureではold activeを維持する。observable commit後のregistry/locator partial phaseはoldへrollbackせずoperation IDでroll-forward / repairまたはfail closedに収束し、二重active・二重spawn・state split-brainを起こさない。
- [ ] rollover 中も control/new spawn は active generation だけ、terminal operation は exact owner generation だけが実行し、late/stale request と event は effect zero になる。
- [ ] restart 後の新規 Agent / generic Terminal は new active が所有し、old resource の exit は durable state/capacity へ一度だけ反映される。
- [ ] old generation は最後の resource/lease/outbox/capacity claim 終了後だけ自動回収され、generation 上限と連続 restart を fail closed に扱う。
- [ ] capability 非対応 old daemon は seamless rollover を開始せず、安全な refusal または明示 cold transition になる。
- [ ] normal stop は live resource がある限り拒否し、明示 force-cold / terminate は結果を確認してから停止する。後続 start は interrupted / explicit resume 契約へ進み、old draining daemon を隠さない。

## 必須 product E2E

in-process `GenerationCoordinator::rollover` unit test だけでなく、shipping binary、2 個の実 daemon process、別 Unix socket、実 PTY child を使う。

- live Agent / generic Terminal あり・なし
- readiness failure、registry / locator各write境界のSIGKILL recovery、observable commitの非rollback
- routing capability無し / 旧client / revision mismatchでhandoff effect zero
- persistent old connection、in-flight spawn/control、internal background producer
- TUI close/reopen、active locator切替、draining endpoint一時不通後もold owner refへ再接続
- restart response / ACK loss、concurrent / repeated restart、generation limit
- G1 exit と G2 spawn の同時実行、late/duplicate old event、capacity release
- normal stop refusal、force-cold、capability 非対応 old build

restart 前後の active/draining PID と generation、Agent/generic child PID・OS start identity、spawn count を記録し、planned restart が provider resume argv を一度も実行しないことを確認する。

## docs / migration

[daemon](../../document/05-daemon.md) と [IPC](../../document/04-ipc.md) は実装済みの現在形だけを記載する。#528/#516/#518/#508/#507 が未完了の間は、shipping restart が stop → fresh start で旧 PTY を継続しない事実、commit unknownでsame-version rebuildを検出できない制約、routing capability完了前はrolloverを有効化しない依存順を示す。legacy state は capability と exact identity を検証できる場合だけ移行し、unknown/corrupt registry や固定 child identity から owner を推測しない。
