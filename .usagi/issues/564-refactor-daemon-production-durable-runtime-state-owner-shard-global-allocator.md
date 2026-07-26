---
number: 564
title: refactor(daemon): production の durable runtime state を owner shard と global allocator へ移す
status: todo
priority: high
labels: [v2, daemon, lifecycle, recovery]
dependson: []
related: [518, 559]
parent: 505
created_at: 2026-07-26T14:36:40.618544+00:00
updated_at: 2026-07-26T14:36:40.618544+00:00
---

## 問題・影響

production の durable runtime state はいまも `agents.json` / `terminals.json` の **whole-snapshot
single-writer store** である。process memory の snapshot を atomic replace するため、2 process が同じ古い
snapshot を load して別々に置換すると lost update になる。planned restart で G1 の exit と G2 の spawn が
同時に起きる状況では、これは state split-brain である。

[#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) の
`usecase::resources`（shard / allocator / launch / drain / retention / migration / fence）は実装済みだが、
合成ルートは駆動していない。`resources::migration::planned_rollover_admission` が要求する
`daemon.child-identity.v1` / `daemon.owner-shard.v1` capability も advertise されていない。

## 対象責務

1. 合成ルートの `FileRuntimeStore` / `FileTerminalStore` / `DurableResourceCensus` を
   owner shard（`shards/<generation>.json`）と global allocator（`allocations.json`）へ置き換える。
   owner generation が 1 つだけの現在も single-writer として正しく動く。
2. legacy な `agents.json` / `terminals.json` は `resources::migration::adopt_legacy` の契約でだけ移行する。
   producer operation を持たない record、OS 検証不能な child identity、重複 resource id は
   `OwnershipUnknown` として採用し、**spawn / kill / capacity release の対象にしない**。unknown / corrupt
   state から owner を推測しない。
3. `daemon.child-identity.v1` / `daemon.owner-shard.v1` を `ServerHello` で advertise する。
   これは `planned_rollover_admission` が predecessor を判定する根拠である。
4. `replacement::ResourceCensus` を shard 由来の live count へ切り替える（`OwnershipUnknown` は live でない）。

## 非対象

seamless rollover の有効化、standby lifecycle、client routing。

## 受入条件

- [ ] 新規 Agent / generic terminal の launch と exit が shard + allocator にだけ記録され、`agents.json` / `terminals.json` は書かれない。
- [ ] legacy state を持つ data directory は一度だけ移行され、移行できない record は `ownership_unknown` として報告され、live に数えられない。
- [ ] `daemon stop` の live-resource refusal（#507）は shard 由来の census で同じ結果になる。
- [ ] capacity pool は kind ごとに独立で、暗黙に合算されない。
- [ ] 同じ producer operation の再送は 1 回だけ効果を持つ（allocator の operation ledger）。
- [ ] `ServerHello` が両 capability を advertise し、`planned_rollover_admission` が `Allowed` になる。

## 必須 product E2E

shipping binary と実 PTY child を使う。

- legacy `agents.json` / `terminals.json` からの移行（採用・unknown 双方）
- launch → exit → capacity release が durable state へ 1 回だけ反映される
- 各 write 境界での SIGKILL recovery（two-object write の片側だけが残った場合は `ownership_unknown`）
- retention ledger の compaction が誤った答えを replay しない
