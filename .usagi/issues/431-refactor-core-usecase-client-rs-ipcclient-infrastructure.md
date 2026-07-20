---
number: 431
title: refactor(core): usecase/client.rs の層違反を解消し IpcClient を infrastructure へ移設する
status: todo
priority: high
labels: [refactor, core, review]
dependson: []
related: [429]
created_at: 2026-07-20T12:00:04.810482+00:00
updated_at: 2026-07-20T12:00:04.810482+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

クリーンアーキテクチャの依存方向（`presentation → usecase → domain ← infrastructure`）に対し、usecase が infrastructure を import する層違反が usagi-core の中枢にある。

## 根拠（検証済み）

- `crates/core/src/usecase/client.rs:21-25` — `use crate::infrastructure::ipc::{…}`（usecase → infrastructure の import）。
- 同ファイル :412 `pub struct IpcClient<S>`、:421 `impl<S: Read + Write> IpcClient<S>`（connect/handshake）、:483 `impl … DaemonClient for IpcClient<S>`（フレームループ）— **wire プロトコル実装そのもの**が usecase 層にある。
- ファイル全体が `#![coverage(off)]`（:7）で、純関数（`decode_pr_snapshot` :141 等）まで除外されている（棚卸しは #429）。

## 問題

usecase 層が wire 実装に依存し、依存方向の規約（02-architecture / 06-conventions）に反する。IPC の変更が usecase に波及し、逆に usecase のテストが wire details を引きずる。

## 改善案（要検討）

- `DaemonClient` trait と request/reply の語彙（純データ型・decode 系純関数）だけを usecase に残す。
- `IpcClient` 本体（handshake・フレームループ）を `infrastructure/ipc/` へ移設し、合成ルートで注入する。
- 移設後に coverage(off) を棚卸しする（#429 とセットで差分最小化）。

## 受け入れ条件

- [ ] `usecase/client.rs` に `infrastructure::ipc` への import が存在しない。
- [ ] `IpcClient` が infrastructure 層にあり、usecase は trait 越しに使う。
- [ ] 既存の client 挙動（handshake・エラー写像）が回帰しない。coverage 100% を維持。
