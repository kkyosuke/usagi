---
number: 559
title: feat(daemon): standby serve と owner shard を配線して seamless rollover を有効化する
status: todo
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: []
related: [209, 221, 275, 350, 492, 507, 508, 515, 516, 518, 528, 550]
parent: 505
created_at: 2026-07-26T13:20:50.130410+00:00
updated_at: 2026-07-26T13:20:50.130410+00:00
---

## 問題・影響

[#507](./507-fix-daemon-planned-replacement-operation-live-runtime-cold.md) は shipping の planned replacement を
1 本の durable operation（`usagi-daemon` の `usecase::replacement`）へ集約し、live runtime を壊す cold transition を
既定で拒否するところまでを閉じた。しかし **seamless rollover 自体はまだ production から起動できない**。
`replacement::seamless_refusal` が durable registry を読んで返す理由は、現在の build では常に
`no generation registry` である。

理由は 3 つの production 配線が欠けていることであり、いずれも pure な authority としては実装済みである。

| 欠けている配線 | 実装済みの authority | 現在の production |
|---|---|---|
| standby process を起動して registry へ登録する `serve` の lifecycle | [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の registry / standby / handoff / admission | `serve` が process lifetime にわたり `daemon.lock` を保持するため、同じ data directory に 2 個目の daemon を起動できない |
| owner generation ごとの runtime shard と global allocator | [#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) の shard / allocator / exit handoff | `agents.json` / `terminals.json` を process memory から whole snapshot で atomic replace する single-writer store。G1 exit と G2 spawn の同時 write は lost update になる |
| client が `TerminalRef.daemon_generation` で owner endpoint を選ぶ経路 | [#508](./508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) の `usagi_core::usecase::owner_routing`（`OwnerRouter` / `merge_inventory` / `GenerationLinks`） | 合成ルートと TUI は `connect_current` だけで接続し、`OwnerRouter` を使わない |

この 3 つが揃うまで seamless rollover を有効化すると、旧 PTY が生存しても到達不能・uncollectable になる。
capability は wire 上すでに双方向で advertise されている（server / client とも `owner-generation-routing.v1`）ため、
残るのは配線だけである。

## 対象責務

1. `serve` を role-aware にする。`daemon.lock` を「1 process 1 data directory」の権威から
   「registry role の権威」へ置き換え、standby が private endpoint（`SecureUnixListener::bind_private`）を
   bind して registry へ standby として登録し、readiness 後に `verified_build` を立てられるようにする。
   単一インスタンス性は registry の active role と workspace fence で保つ。
2. production の durable runtime state を owner shard / global allocator へ移行する。legacy な
   `agents.json` / `terminals.json` は [#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md)
   の migration 契約でだけ移行し、unknown / corrupt state から owner を推測しない。
3. 合成ルートと TUI の client を `OwnerRouter` に載せる。control / launch は active、terminal operation は
   exact owner generation、inventory は merge へ振り分ける。draining endpoint の一時不通は
   `reconnecting` として保持し、verified retirement まで tab を回収しない。
4. `replacement` の preflight を `authority::routing::admit_rollover` と `authority::standby` へ接続し、
   `SeamlessRollover` を `ReplacementPlan` の第 3 の結果として追加する。`execute_gated_rollover` /
   `collect_retired` を shipping lifecycle から駆動する。
5. old generation は owned resource・lease・outbox・capacity claim がすべて 0 になった後だけ
   endpoint / process / registry entry を回収する。generation 上限と連続 restart は fail closed に扱う。

## 非対象

daemon crash / SIGKILL / OS reboot 後に旧 PTY master fd を回収することは対象外とする。broker / Unix FD handoff は
#221、provider-native conversation resume は #503〜#510 の契約に従う。planned restart では provider CLI を
再起動せず、同じ PTY / child process を旧 draining owner が継続する。

## 受入条件

- [ ] shipping `usagi daemon restart` は new active の readiness 後に authority を handoff し、live terminal を持つ old daemon を draining として残す。provider resume argv は一度も実行されない。
- [ ] handoff 後も TUI close/reopen、client reconnect、active locator 切替をまたいで old `TerminalRef` は owner endpoint へ到達でき、新規 control/launch だけが active へ送られる。
- [ ] routing capability / registry revision を確認できない client、旧 build、partial deployment では rollover を開始せず、old active/current と live PTY を維持した typed refusal になる。
- [ ] start/hydrate/bind/readiness と authority commit 前の failure では old active を維持する。observable commit 後の registry/locator partial phase は roll-forward / repair または fail closed へ収束し、二重 active・二重 spawn・state split-brain を起こさない。
- [ ] rollover 中も control/new spawn は active generation だけ、terminal operation は exact owner generation だけが実行し、late/stale request と event は effect zero になる。
- [ ] restart 後の新規 Agent / generic Terminal は new active が所有し、old resource の exit は durable state/capacity へ一度だけ反映される。
- [ ] old generation は最後の resource/lease/outbox/capacity claim 終了後だけ自動回収され、generation 上限と連続 restart を fail closed に扱う。
- [ ] `daemon stop` の live-resource refusal（#507 で実装済み）は変わらず、force-cold は結果を確認してから停止する。

## 必須 product E2E

shipping binary、2 個の実 daemon process、別 Unix socket、実 PTY child を使う。

- live Agent / generic Terminal あり・なし
- readiness failure、registry / locator 各 write 境界の SIGKILL recovery、observable commit の非 rollback
- routing capability 無し / 旧 client / revision mismatch で handoff effect zero
- persistent old connection、in-flight spawn/control、internal background producer
- TUI close/reopen、active locator 切替、draining endpoint 一時不通後も old owner ref へ再接続
- restart response / ACK loss、concurrent / repeated restart、generation limit
- G1 exit と G2 spawn の同時実行、late/duplicate old event、capacity release

restart 前後の active/draining PID と generation、Agent/generic child PID・OS start identity、spawn count を記録する。

## docs / migration

[daemon](../../document/05-daemon.md) の [planned replacement](../../document/05-daemon.md#planned-replacement) と
[IPC](../../document/04-ipc.md) を実装済みの現在形で更新する。`seamless_refusal` の表から解消した variant を落とし、
cold transition だけが残る条件を正しく書き直す。legacy state は capability と exact identity を検証できる場合だけ移行する。
