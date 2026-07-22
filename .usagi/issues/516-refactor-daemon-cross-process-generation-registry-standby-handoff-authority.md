---
number: 516
title: refactor(daemon): cross-process generation registry と standby handoff authority を実装する
status: todo
priority: high
labels: [review, v2, daemon, lifecycle, ipc, generation, recovery]
dependson: [514, 515]
related: [209, 221, 492, 507]
parent: 507
created_at: 2026-07-22T11:30:17.999672+00:00
updated_at: 2026-07-22T11:41:38.542224+00:00
---

## 問題・根拠

#507 の planned restart は 2 daemon process を一時的に共存させる必要があるが、現状はその前提となる cross-process authority がない。

- `daemon.lock` は process lifetime の排他 lock であり、旧 generation が生存中に standby generation を起動できない。
- `SecureUnixListener::bind` は bind と同時に `current.json` を公開するため、standby の private endpoint 準備と active authority commit を分離できない。
- `GenerationCoordinator::rollover` は production lifecycle から呼ばれておらず、`GenerationSnapshot` も Agent runtime store 内の snapshot に留まる。generic terminal を含む全 resource の cross-process authority ではない。
- accept loop の停止後も既存 IPC connection は dispatch を継続できる。接続時だけの判定では role 変更後の stale connection が spawn/control effect を発生させ得る。

この状態で endpoint 切替だけを先に実装すると、二重 active、旧 connection 経由の late spawn、半端な locator 公開を防げない。

## 対象責務

cross-process の generation registry と admission fence を、#507 の shipping lifecycle より先に実装する。

1. durable registry に generation ID、role（`standby | active | draining | retired`）、endpoint、process identity、operation ID、revision を保持し、schema/version と CAS で更新する。
2. single active、不正な role transition、stale revision、未知 schema/corrupt record を fail-closed にする。
3. 新 generation は private endpoint を bind し、side-effect-free な standby readiness を完了するまで `current` authority を変更しない。standby readiness は runtime store の破壊的 reconcile/save、supervisor tick、decision/PR worker 起動、spawn その他の mutation を行わない。owner shard の read-only hydrate と activation は #518 の契約を使う。
4. readiness 完了後、registry の active transition と locator 公開を crash-safe な一つの handoff protocol として commit する。失敗時は旧 active を維持し、standby を回収可能にする。
5. rollover operation を durable/idempotent にし、concurrent restart、ACK loss、再試行、generation 上限を一つの結果へ収束させる。
6. IPC request ごとに最新 role/revision と resource owner を検証する。connection の確立済みという事実を authority としない。
7. `active` だけが control operation と新規 spawn を受理する。`draining` は自 generation が所有する terminal の attach/input/resize/resync/exit/kill と必要な read/inventory だけを受理し、他 generation・新規作成・control mutation は effect zero で拒否する。`standby/retired` は mutation を受理しない。
8. role 変更時は accept loop だけでなく、既存 connection、in-flight request、supervisor/decision/PR refresh 等の internal producer を fence する。active-only work は durable reservation より前に role/revision 付き RAII admission lease を取得し、external effect と durable commit の完了まで保持する。`active → draining` は新規 lease と active-only background worker を先に閉じ、既存 lease / worker が 0 になるまで待ってから registry / locator handoff を commit する。effect 後の再検証だけで既発生 spawn を取り消せるとは扱わない。owner-terminal PTY observer/command は別 lease で継続し、collection は lease 発行停止と 0 確認後だけ許可する。
9. `retired` への遷移では既接続 stream handle を shutdown して frame read を解除し、保持した client worker JoinHandle をすべて join してから endpoint/process を回収する。client thread の JoinHandle を破棄したまま count だけ待たない。
10. legacy single-generation state は、#514 の exact process identity と #515 の crash-safe locator 条件を満たす場合だけ active 1 件へ移行し、所有者を推測しない。

#514 の process identity、#515 の locator/temp recovery を前提にする。owner runtime の永続化方式と exit/capacity の移送は別 issue で扱う。

## 非対象

- shipping `usagi daemon restart` / build-update / stop の切替
- Agent/Terminal runtime store の generation shard 化
- draining owner から active への exit/capacity event handoff
- crash/SIGKILL 後の PTY fd 回収（#221）

これらの production 統合と実 PTY E2E は親 #507 に残す。

## 受入条件

- [ ] durable registry は全 role transition と single-active invariant を CAS で検証し、stale writer、未知 schema、corrupt record を effect zero で拒否する。
- [ ] standby endpoint の bind/readiness は active locator を変更せず、runtime store reconcile/save、worker/tick、spawn/control mutation を一度も行わない。active commit 完了後だけ新 endpoint が current になる。
- [ ] hydrate、bind、readiness、registry commit、locator publish の各 fault injection で old active が利用可能なままになり、二重 active・半端な current を残さない。
- [ ] concurrent/repeated rollover と ACK loss は同一 operation/result へ収束し、generation 上限を越えて process を増やさない。
- [ ] handoff 前から開いている旧 connection は role 変更後に control/spawn effect を発生できない。
- [ ] draining endpoint への direct connection は exact owner resource の terminal operation だけ成功し、他 owner/resource は拒否される。
- [ ] active-only admission lease は role close 後に新規発行されず、handoff は既存 lease / background worker 0 の後だけ commit する。lease は reservation・external effect・durable commit を覆い、handoff 境界で late spawn/mutation を残さない。
- [ ] draining owner-terminal lease は exact owner operation だけに発行され、collection は lease 0 と新規発行停止を確認する。
- [ ] retired は既接続 stream をunblockし、全client workerをjoinしてからendpointを回収する。
- [ ] legacy migration は exact identity/locator が検証できない場合に fail-closed となる。

## 必須テスト

- registry state machine/CAS/operation idempotency の deterministic unit test
- 2 server process・別 Unix socket を使う active/standby/draining integration test
- 既存 persistent connection からの late spawn/control rejection
- bind/readiness/registry write/locator publish/ACK loss の fault injection
- concurrent restart、stale revision、unknown schema、corrupt/truncated record、generation limit
- handoff 中の admission lease close / drain barrier、effect 中 handoff、commit 中 handoff
- supervisor / decision / PR refresh 等internal producerのepoch close / join
- standby readiness が runtime reconcile/save、worker/tick、spawnを呼ばない side-effect recorder
- connected stream shutdownとclient worker joinを含むretired collection

## 依存関係

```text
#514 exact process identity ─┐
                             ├─> 本 issue ─> owner-generation store/handoff issue ─> #507 ─> #508
#515 locator recovery ───────┘
```
