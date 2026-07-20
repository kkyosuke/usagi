---
number: 446
title: refactor(daemon): IPC 層の重複（admit/admit_dispatch ~100 行・端末アクション 6 分岐・geometry 検証）を共通 trait＋変換関数へ統合する
status: todo
priority: high
labels: [refactor, daemon, review]
dependson: []
related: [445]
created_at: 2026-07-20T12:03:49.697159+00:00
updated_at: 2026-07-20T12:03:49.697159+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/agent_ipc.rs` — `admit_dispatch`（:417-517）と `admit`（:519-638、`#[allow(clippy::too_many_lines)]` 付き）が ~100 行重複。
- 端末アクションの 6 分岐が二重: `agent_ipc.rs` の `dispatch_terminal`（fn :810、アーム :820-886、catch-all :888-891）vs `terminal_ipc.rs:211-295`（Attach :211 / Resume :217 / Resync :235 / Resize :241 / Detach :257 / Input :269-276）。
- geometry 検証が完全重複: `agent_ipc.rs:1015` `terminal_geometry` vs `terminal_ipc.rs:322` `geometry` — ロジックはバイト同一（`(cols>0 && rows>0).then_some(Geometry{...}).ok_or_else(…)`）。

## 問題

agent_ipc.rs は 2,101 行に肥大し、IPC の検証・写像の変更が常に 2 箇所修正になる。片側だけの修正でプロトコル挙動が面間で割れる。

## 改善案（要検討）

- admission・端末アクション写像・geometry 検証を共通 trait＋変換関数に抽出し、agent/terminal 両 IPC が共有する。
- Coordinator 統合（#445）とセットで進めると agent_ipc.rs が大幅に痩せる。

## 受け入れ条件

- [ ] admit 系・端末アクション・geometry の重複が解消され、共通実装をテストが直接覆う。
- [ ] 両面の IPC 挙動が回帰しない。coverage 100% を維持。
