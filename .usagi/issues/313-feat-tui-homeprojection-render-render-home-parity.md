---
number: 313
title: feat(tui): HomeProjection に旧 render 経路の描画素材を投影し render_home の parity を固定する
status: todo
priority: high
labels: [tui, render, parity]
dependson: []
related: [258]
parent: 258
created_at: 2026-07-17T14:21:33.157538+00:00
updated_at: 2026-07-17T14:21:33.157538+00:00
---

## 目的

#258（実端末 Workspace runtime の controller 経路移行）の第 1 段階。旧 `Workspace` view だけが持つ描画素材を `HomeProjection` のビルダー入力に移し、`render_home` が旧 `render_with_skeleton_frame` と同等のフレームを出せることを parity golden で固定する。runtime のループには触れない。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.1 / §4.4 / §5 PR1。

## スコープ

- `HomeProjection::with_git_diffs(&BTreeMap<SessionId, GitDiff>)` を追加し、sidebar の差分列を描画する。
- `HomeProjection::with_terminal_view(...)` を追加し、live terminal の viewport 行と terminal feedback を右ペインに描画する。
- `HomeProjection::with_metrics(DaemonMetrics)`（mascot sidecar 計測表示）は session `home-metrics-projection` が別 PR で実装中。未着手のまま残っていれば本 issue に取り込み、先行して merge 済みなら対象外とする。
- parity golden テスト: 代表 state（empty / 多数 session / pending / live terminal / Closeup / tiny geometry / CJK）で旧 render と `render_home` の strip 済みフレームを比較して固定する。
- `AppState` には素材を持ち込まない（毎フレーム外部から来る描画素材は projection のビルダー入力とする）。

## 対象外

- runtime ループ（`drive_workspace_*` / `step_*`）の接続変更。
- 右ペイン tab の可視性・layout の変更。

## 完了条件

- `render_home` が旧経路と同等の情報量（差分列・live terminal 行・feedback・metrics）を描ける。
- 両経路のフレーム一致が golden テストで CI に固定される。
- coverage 100% を維持する。
