---
number: 622
title: refactor(daemon): TerminalOwner port を usecase 境界へ移して presentation 依存逆流を解消する
status: done
priority: medium
labels: [refactor, daemon, ipc, terminal, architecture, clean-architecture, review]
dependson: []
related: [621]
created_at: 2026-08-02T22:56:43.500348+00:00
updated_at: 2026-08-03T00:03:34.732400+00:00
---

## 問題 / 責務違反

daemon の presentation 層が定義した port を usecase 層が import・実装しており、正本の依存方向が逆流している。

- `crates/daemon/src/presentation/ipc.rs::TerminalOwner` が terminal request / inventory / completed inventory / disconnect の actor port を定義する。
- `crates/daemon/src/usecase/terminal_ipc.rs` は `crate::presentation::ipc::TerminalOwner` を import し、`GenericTerminalRuntime` に実装する。
- `crates/daemon/src/usecase/agent_ipc.rs` も同じ presentation trait を importし、`SharedTerminalOwner` の generic/Agent merge をその trait 上で実装する。
- 合成ルート `src/runtime/daemon.rs::SharedTerminal` も presentation trait を直接実装して production owner を束ねる。

`document/02-architecture.md#クリーンアーキテクチャとの対応` の正本は `presentation → usecase → domain ← infrastructure` とし、IPC request dispatch / response shaping は presentation、daemon logic は usecase、実 IO の結合は composition root と定める。現在は `usecase → presentation` の静的依存が成立している。

## 発生条件 / 影響

`TerminalOwner` の JSON payload、negotiated `SnapshotWire`、inventory shape、connection lifecycle のいずれかを presentation 都合で変更すると、generic terminal と Agent runtime の usecase 実装まで同時に変更が必要になる。

- usecase の runtime ownership / merge policy を presentation adapter から独立して fake port で検証できず、protocol JSON と application behavior の変更単位が結合する。
- 新しい daemon presentation adapter を追加すると usecase が既存 IPC presentation trait に固定され、composition root で adapter を差し替える設計にならない。
- `TerminalOwner::request` が `serde_json::Value` と negotiated wire mode を usecase 実装へ渡すため、request decode と response shaping の責務が presentation 境界から usecase へ漏れる。

これは動作中の terminal/Agent owner 全経路（launch、attach、input、inventory、disconnect）が通る production wiring の責務違反であり、未使用コードだけの問題ではない。

## 修正方針

- terminal owner の application input port と typed request/result を daemon usecase 側へ置く。generic terminal / Agent merge はこの usecase-owned port を実装する。
- `serde_json::Value` の decode/encode、`TerminalAction` と typed request の照合、negotiated `SnapshotWire` に応じた response shaping は `presentation::ipc` adapter に残す。
- connection-scoped disconnect と inventory fan-out の ownership は typed port で明示し、現在の fail-closed routing / workspace scope / visibility CAS 契約を維持する。
- composition root は usecase owner と presentation adapter、PTY/store ports を結合するだけにする。
- production module から `crate::presentation` を参照する `usecase` import をなくす。

## 必要な回帰テスト

- launch / attach / resync / resize / input / input outcome を typed usecase port と IPC adapter の両境界で固定する。
- generic terminal と Agent terminal の inventory / completed inventory が同じ scope filter と merge 結果を維持する。
- disconnect が両 owner へ一度ずつ伝播し、connection-local input ledger と rollover fence の既存挙動を維持する。
- malformed action/payload と unsupported snapshot negotiation が usecase effect 前に presentation で拒否される。
- architecture test または静的検索で `crates/daemon/src/usecase/**` から `crate::presentation` への production import を禁止する。
- composition root の production wiring test が新しい adapter 経路を直接通る。

## 受入条件

- daemon usecase が presentation module の型・trait を importしない。
- JSON/wire negotiation mapping は presentation、terminal ownership policy は usecase、実 adapter 結合は composition root に分離される。
- terminal/Agent の production behavior と protocol wire compatibility が維持される。
