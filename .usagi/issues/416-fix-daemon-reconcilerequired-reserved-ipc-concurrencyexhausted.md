---
number: 416
title: fix(daemon): ReconcileRequired/Reserved スロットの解放経路を IPC に配線する（ConcurrencyExhausted 恒久化の解消）
status: todo
priority: high
labels: [fix, daemon, review]
dependson: []
related: [411, 415]
created_at: 2026-07-20T11:56:13.910926+00:00
updated_at: 2026-07-20T11:56:13.910926+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/runtime.rs:504-515` と `generic_terminal.rs:315-326` — `occupied_slots` は `Reserved | Running | ReconcileRequired(_)` を数える。
- スロット上限 16: `agent_ipc.rs:238` `RuntimeCoordinator::new(16, 64 * 1024, 64)`、`terminal_ipc.rs:109` `GenericTerminalCoordinator::new(16, ...)`（いずれも `crates/daemon/src/usecase/` 配下）。
- スロットを解放する `reconcile` へ到達する唯一の経路は `Orchestrator::reclaim`（orchestration.rs:358、`runtime.reconcile(...)` 呼び出しは :383）だが、**reclaim の本番呼び出し元はゼロ**（参照は自テスト :613/:623/:633 のみ）。他の `reconcile` 呼び出しもすべて `#[cfg(test)]` 内。

## 問題

spawn 失敗・プロセス消失などで `ReconcileRequired` に落ちたスロットを解放する手段が本番に存在しない。事故が 16 回累積すると daemon 再起動まで `ConcurrencyExhausted` が恒久化し、以後の agent/terminal 起動がすべて拒否される。

## 改善案（要検討）

- 管理者向けの reconcile/reap verb を daemon IPC に配線する（`Orchestrator::reclaim` の配線が有力。未配線コードの意思決定 issue #411 と整合させる）。
- あわせて自動回収（プロセス生存確認に基づく定期 reconcile）も検討する。

## 受け入れ条件

- [ ] ReconcileRequired スロットを daemon 再起動なしで解放できる経路が本番 IPC に存在する。
- [ ] スロット枯渇→解放→再 launch の一連がテストで固定されている。
- [ ] coverage 100% を維持する。
