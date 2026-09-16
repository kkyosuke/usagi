---
number: 763
title: refactor(core): IPC protocol 語彙を infrastructure::client から protocol module へ分離する
status: todo
priority: medium
labels: [v2, refactor, core, ipc]
dependson: []
related: [757]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

daemon IPC の request / reply 語彙（`DaemonRequest` 28 variants、`SessionAction` 25 variants など public 型 34）は
`crates/core/src/infrastructure/client.rs`（production 約 2,100 行）に client connection state machine（`PolicyClient`、
retry、deadline）と同居している。

この語彙は `crates/daemon/src/usecase`（43 参照）、`crates/cli/src`（46 参照）、`crates/tui/src`（21 参照）から
`usagi_core::infrastructure::client::…` の path で使われる。`document/02-architecture.md` は「実行時通信は usagi-core の
IPC プロトコル型を介する」と定義するが、その型が `infrastructure` に置かれているため、usecase 層が別 crate の
infrastructure module へ依存する形になり、依存 matrix 上は許容されていても「protocol 契約」と「接続実装」の変更が
同じファイル・同じ review に混ざる。

## 方針

- request / reply / action / hello などの wire 契約を `usagi_core::protocol`（または `domain::ipc`）へ移し、
  `serde` derive だけを持つ pure な型に限る。
- `infrastructure::client` は接続 state machine と retry policy に絞り、語彙は re-export で後方互換を保ちながら
  呼び手を新 path に移す。
- `tests/architecture.rs` の層 matrix に protocol module を加え、usecase 層からの `infrastructure::client` 直接参照を
  禁止方向に更新する。

## 完了条件

- `crates/{daemon,tui,cli}/src` の usecase / presentation から `usagi_core::infrastructure::client` への参照が
  connection 実装を注入する合成ルート以外に無い。
- `document/04-ipc.md` の型の所在が新 module を指す。
