---
number: 445
title: refactor(daemon): RuntimeCoordinator と GenericTerminalCoordinator のコピペを record 型 generic の単一 Coordinator に統合する
status: todo
priority: high
labels: [refactor, daemon, review]
dependson: []
related: [425]
created_at: 2026-07-20T12:03:29.584593+00:00
updated_at: 2026-07-20T12:03:29.584593+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/runtime.rs` の `RuntimeCoordinator`（struct :192、impl :198）と `generic_terminal.rs` の `GenericTerminalCoordinator`（struct :87、impl :92）は、スロット管理・admission・replay・exited 処理がほぼ全文コピペ。
- 既に挙動が**無根拠に非対称**: `replay_from` の状態ゲートは generic 側が `replayable`（generic_terminal.rs:242-251、:377-386 で `Running | Exited` のみ許可）なのに対し、agent 側は `self.record(runtime)?`（runtime.rs:432-441）で**任意状態を許可**。
- エラー enum と IPC 写像も両系統で二重定義されている。

## 問題

コピペ 2 実装は既にドリフトしており（replay ゲート差）、修正のたびに片側だけ直る事故が起きる。replay 冪等意味論の面間不一致（#425）も同根。

## 改善案（要検討）

- record 型を generic パラメータにした単一 `Coordinator<R>` に統合する（agent record / generic terminal record を差し込む）。
- replay 状態ゲートの正しい仕様を決めて 1 箇所に実装する（非対称の解消）。
- エラー enum・IPC 写像の二重も同時に解消する（IPC 層の重複統合 issue とセットで実施可）。

## 受け入れ条件

- [ ] Coordinator が 1 実装になり、agent/terminal 両面がそれを使う。
- [ ] replay ゲートの仕様が統一され、テストで固定されている。
- [ ] coverage 100% を維持する。
