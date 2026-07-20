---
number: 413
title: refactor(core): domain::id と infrastructure::ipc の二重 ID 型を wire 用に一本化する
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-07-20T11:55:09.355698+00:00
updated_at: 2026-07-20T11:55:09.355698+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/core/src/domain/id/mod.rs`: `resource_id!` マクロ（:68、UUID backing）で `ClientId`（:130）・`ConnectionId`（:131）・`RequestId`（:132）・`DaemonGeneration`（:134）・`ProtocolVersion`（:199）を定義。
- `crates/core/src/infrastructure/ipc/mod.rs`: `string_id!` マクロ（:25、String backing）で同名の `ClientId`（:28）・`RequestId`（:29）・`DaemonGeneration`（:34）・`ConnectionId`（:35）・`ProtocolVersion`（:60）を別系統で定義。
- 橋渡し: `crates/daemon/src/presentation/ipc.rs:216` が `usagi_core::domain::id::RequestId::parse(&request_id.0)` と**文字列パースで変換**している。
- `domain::id::ProtocolVersion` は本番利用者ゼロ（`usecase/client.rs:21-24` は infrastructure::ipc 側を import。参照は `domain/id/tests.rs` のみ）。

## 問題

同名 ID 型が 2 系統あることで、import 誤りと文字列パースによる実行時変換（型安全性の放棄）が発生している。デッドコード（domain 側 ProtocolVersion）も残存。

## 改善案（要検討）

- wire（IPC）で使う ID 型を 1 系統に統一する（domain の typed ID を wire でも使い infrastructure 側の string_id を落とすか、wire 語彙は infrastructure に限定し domain 側の未使用型を削除するか、方向を決める）。
- `presentation/ipc.rs:216` の文字列パース橋渡しを型変換なしで通るようにする。
- `domain::id::ProtocolVersion` のデッドコードを解消する。

## 受け入れ条件

- [ ] `ClientId`/`RequestId`/`ConnectionId`/`DaemonGeneration`/`ProtocolVersion` の定義が 1 系統になる（または残す 2 系統の役割が型で明確に分離され、文字列パース橋渡しが消える）。
- [ ] 利用者ゼロの ID 型が残っていない。
- [ ] coverage 100% を維持する。
