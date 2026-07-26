---
number: 563
title: feat(daemon): serve を role-aware にして standby process を registry へ登録する
status: todo
priority: high
labels: [v2, daemon, lifecycle, recovery]
dependson: []
related: [516, 559]
parent: 505
created_at: 2026-07-26T14:36:17.504208+00:00
updated_at: 2026-07-26T14:36:17.504208+00:00
---

## 問題・影響

[#559](./559-feat-daemon-serve-durable-generation-registry-active-generation.md) で `serve` は自分の generation を durable
registry へ **active** として登録するようになった。`generations.json` は production に存在し、
`replacement::seamless_refusal` は `no generation registry` ではなく `no verified standby` を返す。

残っているのは **standby 側の lifecycle** である。現在 `serve` は role を 1 つしか持たない。
`daemon.lock` を process lifetime にわたり保持するため、同じ data directory に 2 個目の daemon を
起動できず、authority を渡す先の standby process が存在しない。

## 対象責務

1. `serve` を role-aware にする。`daemon.lock` を「1 process 1 data directory」の権威から
   「registry role の権威」へ置き換える。単一インスタンス性は registry の active role と workspace fence で保つ。
2. standby role の `serve` は private endpoint（`SecureUnixListener::bind_private`）を bind し、
   `authority::standby::prepare_standby` で registry へ standby として登録し、readiness 後に
   `verified_build` を立てる。`current.json` は変更しない。
3. standby は admission barrier（`authority::admission`）を standby role で開始し、
   active になるまで control work を受理しない。
4. standby process の起動は [#559 の残件 D](./566-feat-daemon-replacement-seamless-rollover.md) が
   駆動する。本 issue は「起動されたら安全に standby になれる」ところまでを閉じる。

## 非対象

authority handoff の実行（`execute_gated_rollover` の駆動）は別 issue。owner shard への移行も別 issue。

## 受入条件

- [ ] 同じ data directory に active 1 + standby 1 の 2 process が同時に生存でき、standby は `current.json` を書かない。
- [ ] standby の readiness は side-effect free である（locator write・runtime store reconcile / save・supervisor tick・worker start・spawn のいずれも起こさない）。
- [ ] artifact identity が unknown / mismatch の standby は `verified_build` を立てず、old active と `current` を変えない。
- [ ] active が生存している間は 2 個目の **active** を起動できない（registry の active role と workspace fence が拒否する）。
- [ ] standby の start / bind / readiness いずれの failure でも old active を維持する。
- [ ] generation 上限（2）を超える standby 登録は fail closed になる。

## 必須 product E2E

shipping binary、2 個の実 daemon process、別 Unix socket を使う。

- active 稼働中に standby を起動 → registry に standby entry と `verified_build` が現れ、`current.json` は不変
- standby の readiness failure で old active/current が不変
- 3 個目の generation 登録が generation 限界で拒否される
- standby を kill した後、次の start が registry を fail closed に収束させる
