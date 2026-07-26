---
number: 568
title: feat(daemon): serve を durable generation registry の active generation として登録する
status: done
priority: high
labels: [v2, daemon, lifecycle, recovery]
dependson: []
related: [507, 508, 515, 516, 550, 561]
parent: 559
created_at: 2026-07-26T15:01:46.313645+00:00
updated_at: 2026-07-26T15:01:53.955560+00:00
---

## 問題・根拠（コード調査で確定）

[#561](561-refactor-daemon-serve-role-aware-standby-process.md) は `serve` を role-aware にして
**standby** を起動できるようにする。その前段として、**active 側が registry に登録されていない**という
更に基本的な欠落がある。

- `crates/daemon/src/usecase/authority`（[#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)）の
  registry / standby / handoff / admission は実装済みで、filesystem adapter
  （`infrastructure::generation_registry`）も存在する。
- しかし合成ルートが `usagi_daemon::infrastructure::generation_registry` から呼ぶのは
  `read_registry_document` 1 つだけで、**`generations.json` を書く production 経路が存在しない**。
- したがって `replacement::seamless_refusal` は build を問わず常に `no generation registry` を返す。
  refusal が観測ではなく**定数**になっている。
- client 側の `TrustedGenerationDirectory`（[#508](508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md)）も
  registry が無いので常に locator へ退化し、production で owner routing を検証できない。
- handoff protocol は authority を渡す先を registry から名指すが、**渡す元**すら登録されていない。

## この issue を分けた理由

registry に渡す元が登録されていない状態では、standby も rollover も載せる土台が無い。一方これは
**standby lifecycle とは独立に、単一 generation のままで完結する**変更であり、`daemon.lock` の役割を
移す（#561 の本体で、最も安全性に直結する部分）必要がない。したがって #561 の前段として単独でレビューできる。

## 既存 issue との境界

- [#561](561-refactor-daemon-serve-role-aware-standby-process.md) — `daemon.lock` の役割移転、standby の
  bind / register / readiness、read-only hydrate。**本 issue は active 側の登録だけ**で、role は増やさない。
- [#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)（done）— registry の
  role / transition / handoff / recovery の**判定は正本がそちら**。本 issue は production lifecycle の配線と、
  handoff を経由しない first activation の document transition を足すだけである。
- [#515](515-fix-daemon-current-locator-crash-safe-atomic-publish.md)（done）— locator の crash-safe publish。
  bind と publish を分ける本 issue でも、locator の書き込み契約は変えない。

## やること

1. `serve` の endpoint 公開を **bind** と **claim** の 2 段に分ける。bind は endpoint を *応答する* 状態にし
   （`SecureUnixListener::bind_private`）、claim は registry へ登録してから `current.json` を publish して
   endpoint を *発見可能にする*。この順序で「registry entry が誰も accept していない endpoint を名指す」
   「published locator が registry の知らない generation を名指す」の両方が起こらなくなる。
2. `usecase::authority::activation` を追加する。claim は recover → activate_first（registry CAS）→
   locator publish の 1 本の flow で、write 順序は handoff protocol の `from = None` 版である。
3. `RegistryDocument::activate_first` / `retire_self` を足す。retained generation が 1 つでもある registry は
   handoff protocol の領分なので `authority_retained` で effect zero に拒否する。前 incarnation の
   retired entry は同じ CAS で捨て、restart 1 回ごとに document が太らないようにする。
4. 正常終了時は locator → registry entry の順に authority を返却する。
5. artifact identity が unknown な build も activation できる（`verified_build` は unknown のまま記録し、
   rollover successor にはならない）。

## 受入条件

- [x] shipping daemon の起動で `generations.json` が現れ、active 1 件が `current.json` と同じ generation / endpoint を名指す。
- [x] 記録される process identity は `daemon.json` と同じ start identity token であり、後続 start が exact 比較で生存を判定できる。
- [x] `daemon stop` は authority を返却し、`current` が null、entry が `retired` になる。
- [x] 繰り返し restart しても retained generation は常に 1 件で、document は太らない。
- [x] locator publish の前に crash した場合、次の start は authority を証明できないので fail closed（entry を retire）へ収束し、旧 generation の record は残らない。
- [x] 生存している active generation を、別 process の claim が displace しない（`authority_retained` の typed refusal）。
- [x] bind していない状態の claim、canonical でない generation 名の claim は registry を書かずに拒否する。
- [x] endpoint retirement に失敗した場合は authority を返却せず、record を completion fence として残す。
- [x] `replacement::seamless_refusal` は稼働中 daemon に対して `no verified standby` を返す（`no generation registry` は「この data directory で daemon が一度も起動していない」状態だけを指すようになる）。
- [x] カバレッジ 100% を維持し、[document/05-daemon.md](../../document/05-daemon.md) の
      [first activation](../../document/05-daemon.md#first-activation) を実装に合わせて追加する。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::authority` の registry / handoff / recovery が退行しないこと）
- `cargo test -p usagi --bin usagi`（`serve` の lifecycle 配線と合成ルートの adapter）
- **出荷バイナリの結合テスト**: `daemon start` で registry と locator が一致すること、`daemon stop` で
  retire されること、restart を繰り返しても entry が 1 件のままであること。起動は必ず
  [`tests/support/daemon.rs` 経由](../../document/06-conventions.md#結合テストからの-daemon-起動)。
