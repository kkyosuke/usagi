---
number: 319
title: feat(tui): metrics / git diff を BackendEvent 駆動に統合する
status: done
priority: medium
labels: [tui, controller, daemon]
dependson: [315]
related: [258, 313, 314]
created_at: 2026-07-17T14:23:17.883035+00:00
updated_at: 2026-07-18T05:21:05.555598+00:00
---

## 目的

#313 で `HomeProjection` のビルダー入力として暫定的に毎フレーム polling している daemon metrics / git diff を、`BackendEvent` 化して `DaemonBackend`（#314）の drain に統合し、フレームループ内の直接 polling を撤去する。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §8-3。

## スコープ

- `BackendEvent` に metrics / git diff の更新 variant を追加し、`DaemonBackend` が `DaemonMetricsPort` の取得結果（キャッシュ・別スレッド git diff を含む）を event として還流する。
- フレームループ側の `metrics_port.latest()` 直接呼び出しを撤去し、shell の投影キャッシュを event 更新にする。
- 表示自体（`with_metrics` / `with_git_diffs`）は変更しない。

## 完了条件

- metrics / git diff の更新が drain → `update()` → 投影の単方向経路に乗る。
- fake port による還流テストで固定され、coverage 100% を維持する。
