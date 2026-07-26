---
number: 565
title: fix(tui): 合成ルートと TUI の client を OwnerRouter に載せる
status: todo
priority: high
labels: [v2, tui, ipc, recovery]
dependson: []
related: [508, 559]
parent: 505
created_at: 2026-07-26T14:37:08.584201+00:00
updated_at: 2026-07-26T14:37:08.584201+00:00
---

## 問題・影響

client は handshake で `owner-generation-routing.v1` を **無条件に advertise している**
（`usagi_core::usecase::client` の `ClientHello`）。しかし合成ルートと TUI は `connect_current` で
publish 済み locator にだけ接続し、`usagi_core::usecase::owner_routing`（`OwnerRouter` /
`merge_inventory` / `GenerationLinks` / `presence_of`）を一切使っていない。

つまり **advertise は真ではない**。daemon 側の rollover gate（`authority::routing::admit_rollover`）は
この advertise を根拠に「rollover しても client は draining generation へ到達できる」と判断するため、
seamless rollover を有効化した瞬間に、旧 PTY が生存したまま到達不能になる。

## 対象責務

1. 合成ルートの client 経路（`policy_client` / `attached_client` / terminal lane / pump）を
   `OwnerRouter` に載せる。endpoint は `TrustedGenerationDirectory` から取り、client が socket path を
   composeしない。
2. routing を契約どおりに振り分ける。control / launch は active generation、`TerminalRef` を伴う
   terminal operation は exact owner generation、scope inventory は全 generation の merge。
3. draining endpoint の一時不通は `OwnerPresence::Reconnecting` として保持し、tab を回収しない。
   tab を回収するのは owner の authoritative な非 live 応答、または registry からの verified retirement だけ。
4. `GenerationLinks` を per-generation の connection / output cursor として持ち、`current` の publish で
   draining subscription を捨てない。transport failure は socket だけ落として cursor を保持する。
5. generation が 1 つだけの現在も同じ経路で動く（`generations.json` が active 1 件を名指すため、
   router は今日の挙動へ退化する）。

## 非対象

daemon 側の standby lifecycle・owner shard・rollover の有効化。

## 受入条件

- [ ] すべての client 要求が `TrustedGenerationDirectory` 由来の endpoint へ行き、client が endpoint path を組み立てる API が存在しない。
- [ ] unknown / retired / 偽造 generation を名指す `TerminalRef` は typed refusal（`StaleTarget`）になり、active endpoint へ fallback しない。
- [ ] scope inventory は generation ごとの答えを merge し、他 generation の terminal を持ち込む entry と scope 外 entry を落とす。
- [ ] 1 generation だけの状態で、既存の TUI / CLI / MCP の挙動が変わらない。
- [ ] partial merge（片方 unreachable）で tab が消えない。
- [ ] client が routing を実装していない場合にだけ `owner-generation-routing.v1` を advertise しない、という対応が取れている（advertise と実装が一致する）。

## 必須 product E2E

- 実 PTY の TUI で tab close/reopen・client reconnect をまたいで同じ owner ref へ再接続できる
- inventory の merge が重複 tab を作らない
- draining endpoint 一時不通 → 復帰で tab が保持されたまま resync する
