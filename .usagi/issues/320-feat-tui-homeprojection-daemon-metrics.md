---
number: 320
title: feat(tui): HomeProjection に daemon metrics 投影を追加する
status: done
priority: medium
labels: [tui]
dependson: []
related: [258]
created_at: 2026-07-17T14:27:00.081155+00:00
updated_at: 2026-07-17T14:27:15.182066+00:00
---

## 目的

issue #258（Home の描画・入力経路を controller 経路へ一元化）の第一段階 PR1「投影の parity」タスク 1。旧描画経路（`presentation/views/workspace.rs` の旧 `Workspace` view + `render_with_skeleton_frame`）だけが持つ描画素材のうち、**daemon metrics（mascot sidecar の計測表示）** を controller 経路（`HomeProjection` / `render_home`）へ移し、両経路で同一表示になる parity を確立する。

## 背景

v2 TUI の Workspace 画面には描画が二系統ある。実端末は旧経路で動き、controller 経路（`AppState` / `HomeProjection` / `render_home`）はテストからのみ使われている。旧経路は `Workspace::metrics` フィールドと `set_metrics()` を所有し、`presentation/mod.rs::refresh_metrics()` が毎フレーム `DaemonMetricsPort::latest` の結果を渡している。controller 経路の `HomeProjection` には metrics が無いため、mascot sidecar の計測部分を描けない。

## スコープ

- `HomeProjection` に `metrics: Option<DaemonMetrics>` フィールドと、`with_pane` / `with_mascot_speech` と同じ consuming builder 形式の `with_metrics(Option<DaemonMetrics>)` を追加する。
- `render_home`（`home_left_pane`）が、旧 `render_with_skeleton_frame` が metrics から描くのと同じ mascot sidecar 計測表示を出す。旧経路の metric 投影ロジック（`mascot_metrics`）を両経路から共有し、コピーを作らない。
- `AppState`（reducer）には metrics を入れない。毎フレーム外部から来る描画素材は projection のビルダー入力とする確立済みの設計に従う。
- metrics が `None` のときは metrics 導入前と同じ frame を保つ（後方互換）。

## 対象外

- 旧経路・runtime のループ（`presentation/mod.rs` の `drive_workspace_*` / `step_*`）の接続変更。今回は投影と描画のみ。
- daemon metrics 以外の描画素材の移設（PR1 の別タスク）。
- runtime を controller 経路へ一元化する #258 本体の接続。

## 完了条件

- `HomeProjection::with_metrics` で与えた `DaemonMetrics` が `render_home` の mascot sidecar に描かれる。
- 同じ metrics を旧 `Workspace`（`set_metrics`）と `HomeProjection`（`with_metrics`）へ与えたとき、旧 `render_with_skeleton_frame` と `render_home` の計測行（ANSI 込み）が一致する。
- metrics 無しでは既存 golden が変わらない（sidecar 行が出ない）。
- カバレッジ 100% を維持する。

## 検証

- parity テスト: 旧経路と controller 経路の計測行の一致（strip 前後）。
- 後方互換テスト: `with_metrics(None)` が frame を変えず、計測行が出ないこと。
