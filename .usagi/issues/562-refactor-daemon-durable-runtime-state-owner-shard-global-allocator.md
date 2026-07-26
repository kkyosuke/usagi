---
number: 562
title: refactor(daemon): durable runtime state を owner shard と global allocator へ移行する
status: in-progress
priority: high
labels: [review, v2, daemon, runtime, durability, recovery]
dependson: []
related: [518, 526, 555, 559]
parent: 559
created_at: 2026-07-26T13:58:56.757197+00:00
updated_at: 2026-07-26T21:29:36.230484+00:00
---

## 問題・根拠（コード調査で確定）

[#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) は owner generation ごとの
runtime shard と global resource allocator を実装したが、**production の durable state はまだ移行していない**。

production は Agent を `agents.json`、generic Terminal を `terminals.json` へ、process memory から
**whole snapshot の atomic replace** で書く single-writer store のままである。したがって
**G1 の exit と G2 の spawn が同時に書くと lost update になる**。

## この issue を分けた理由

durable state の移行は **crash safety と migration が本体**であり、routing 配線
（[#560](560-feat-tui-client-ownerrouter-owner-generation-routing.md)）や
`serve` の role 化（[#561](561-refactor-daemon-serve-role-aware-standby-process.md)）と混ぜると、
「壊れたときにどの層が原因か」を切り分けられなくなる。移行は単独でレビューし、legacy state からの
migration を単独で検証できる形にする。

## 既存 issue との境界

- [#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md)（done）— shard / allocator /
  exit handoff / migration 契約の**正本はそちら**。判定と契約を再実装しない。本 issue は production をその契約へ載せる配線である。
- [#526](526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md)（done）— final tombstone の retention。
  **retention の budget / eviction は対象外**。ただし #518 が定めた allocator claim / operation ledger の
  retention は本 issue の範囲に含む。
- [#555](555-perf-daemon-pr-identity-pty-hot-path.md)（done）— `pr-inventory.json` は in-process の単一 writer が
  cache 経由で書く形になった。**cross-generation の single writer は #518 の contract に従って本 issue で扱う**
  （#555 はその契約を先取りしていない）。
- **rollover の有効化と 2 process の product E2E は [#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md)**。

## やること

1. Agent / generic Terminal の durable record を owner `DaemonGeneration` ごとの shard へ移す。
   各 process は自 shard だけを書き、別 generation の snapshot を置換しない。
2. capacity / reservation を global allocator へ移し、active と draining の合計上限を一意に管理する。
3. legacy `agents.json` / `terminals.json` を #518 の migration 契約でだけ移行する。
   **unknown / corrupt / 固定 identity の record から owner を推測しない**（fail closed）。
4. `pr-inventory.json` を含む shared writer を owner-local event + single writer か同等の generation fence へ載せる。

## 設計上の判断が必要な点

- **移行の不可逆性**。shard へ移した後に旧 build へ戻せるのか（戻せないなら、その旨と検出方法を決める）。
  部分移行のまま crash した場合の収束先を決める。
- **`pr-inventory.json` の扱い**。#555 で in-process cache + write-through にしたので、
  cross-generation の single writer をどう入れるかは cache の invalidation と直接ぶつかる。
  **draining generation が inventory を書く必要があるのか**をまず決める（書かないなら fence だけで済む）。
- **migration の検証単位**。1 record ずつ検証して移すのか、全体を検証してから一括で切り替えるのか。
  後者なら切り替え中の crash 点を列挙する必要がある。

## 受入条件

- [ ] G1 の exit と G2 の spawn を同時実行しても lost update が無い（barrier 同期の two-writer 回帰テスト）。
- [ ] capacity は全 retained generation で設定上限を超えず、reservation 失敗が spawn effect zero になる。
- [ ] legacy state は exact identity と capability を検証できた record だけ移行され、証明不能な record は
      `identity_unknown` の非 spawnable safe failure になる。
- [ ] 部分移行のまま crash しても、二重 spawn・state split-brain を起こさず収束する。
- [ ] `pr-inventory.json` を含む shared writer に single writer または generation fence がある。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の
      [daemon data directory](../../document/05-daemon.md#daemon-data-directory) と runtime state の記述を更新する。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::resources` / `infrastructure::resource_store` が退行しないこと）
- `cargo test -p usagi --bin usagi`（production store の配線）
- **two-writer interleaving**: G1 exit / G2 spawn を barrier で同期させ、両方の transition が 1 度ずつ残ることを固定する。
- **migration**: legacy snapshot（正常・schema 未知・corrupt・固定 identity）の 4 種から移行して、
  fail closed になるものを固定する。
- store failpoint で部分移行の crash 点を列挙して検証する。
- Rust 差分を含むため fmt / check / clippy / 推奨 test を通し、full gate は PR CI で確認する。
