---
number: 411
title: refactor(daemon): 未配線の将来設計コード（generation/control/orchestration/pr_inventory/metrics）を配線するか削除するか決定する
status: todo
priority: high
labels: [refactor, daemon, review]
dependson: []
related: [405]
created_at: 2026-07-20T11:54:48.519768+00:00
updated_at: 2026-07-20T11:54:48.519768+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

`usagi-daemon` に、自モジュールのテスト以外から参照されない将来設計コードが約 1,600 行分存在する。配線するか削除するかの意思決定が必要。

**注**: `supervisor_runtime.rs`（636 行、`SupervisorRuntime` は本番構築ゼロ）も同類だが、その配線は **#405（supervisor_* の production 配線）が正本**なので本 issue のスコープ外とする。

## 根拠（検証済み）

- `crates/daemon/src/usecase/generation.rs`: `GenerationCoordinator`（struct :106、pub メソッド ~14 個 :116-358）は非テスト参照ゼロ。ただし同ファイルの値型（`ProcessIdentity` / `ProcessObservation` / `SpawnFailure`）は runtime.rs・generic_terminal.rs・orchestration.rs・合成ルート daemon.rs:48 で**利用中**（ファイル全体が死んでいるわけではない）。
- `crates/daemon/src/usecase/control.rs`: 公開関数 `report_phase`（:88）・`resolve_target`（:107）・`advance_prompt`（:128）・`begin_remove`（:150）は本番呼び出しゼロ（`control::AgentPhase` 型のみ orchestration.rs:26 が利用）。
- `crates/daemon/src/usecase/orchestration.rs`: `enable_phase_reporting`（:251）・`report_phase`（:291）・`resume`（:329）・`reclaim`（:358）は本番呼び出しゼロ（テスト :572-586, :613-633 のみ）。`Orchestrator` 自体は配線済み（agent_ipc.rs:159/239、`.launch` :468/580）。
- `crates/daemon/src/usecase/pr_inventory.rs`: `RefreshScheduler` / `refresh_one` / `GhProcessPort` はモジュール外参照ゼロ（`OutputPrProjector` は daemon.rs:701 で配線済み）。
- `crates/daemon/src/usecase/metrics.rs`: `MetricsBroker` は非テスト参照ゼロ（合成ルートの metrics 応答は固定値を返している。関連: 小粒バグ束 issue の「active_subscribers/dropped_updates 固定 0」）。

## 問題

- 未実行コードの維持コスト（テスト・coverage 除外・リファクタ時の巻き添え）。
- `Orchestrator::reclaim` は ConcurrencyExhausted 恒久化の解消（別 issue）に必要な唯一のスロット解放経路であり、「未配線のまま」は運用リスクと直結している。

## 改善案（要検討）

- 機能単位で「配線 or 削除」を決定する。特に:
  - `reclaim` は管理者向け reconcile verb の IPC 配線（別 issue）とセットで配線が有力。
  - `MetricsBroker` は metrics 応答の実値化とセットで判断。
  - `GenerationCoordinator` / `control.rs` 公開関数 / `RefreshScheduler` は利用計画がなければ削除。
- 削除後に daemon の coverage(off) 棚卸し issue（後続）を実施すると差分が小さい。

## 受け入れ条件

- [ ] 上記各機能の配線/削除の決定が記録され、実行されている。
- [ ] 残すコードは本番経路から到達可能で、テストが本番経路を通る。
- [ ] coverage 100% を維持する。
