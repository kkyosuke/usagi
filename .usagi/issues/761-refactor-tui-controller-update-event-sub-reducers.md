---
number: 761
title: refactor(tui): controller の update_event を event family 別の sub-reducer に分ける
status: done
priority: medium
labels: [v2, refactor, tui]
dependson: []
related: [758]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T16:22:57.946361+00:00
---

## 問題

`crates/tui/src/usecase/application/controller.rs` の Home reducer `update_event` は 654 行、
`clippy::cognitive_complexity` 61、top-level の match arm が 39 本あり、全 bounded context の event が
1 つの `match` に集約されていた。`#[allow(clippy::too_many_lines)]` を付けて通していた。

## やったこと

- `AppEvent::Backend(...)` の 15 arm を `update_backend_event(state, event: BackendEvent)` へ分離し、
  `update_event` からは 1 本の委譲 arm にした。
- 両 reducer の大きい arm の本体を、名前の付いた関数へ持ち上げた（`update_workflow_input` /
  `update_workflow_edit` / `update_pane_tab_availability` / `update_operation_result` /
  `update_workflow_backend` / `update_session_lifecycles` / `update_session_snapshot` /
  `update_runtime_phase` / `update_daemon_control_finished` / `update_backend_notice` /
  `update_director_launch_finished` / `update_tick` / `update_live_pane_availability` /
  `update_retained_pane_activated` / `update_resize` / `update_workspace_drawer_focused`）。

| 関数 | before | after |
|---|---|---|
| `update_event` 行数 | 654 | 100 |
| `update_event` cognitive complexity | 61 | 25 未満（lint に出ない） |
| `update_backend_event` 行数 | — | 93 |
| `#[allow(clippy::too_many_lines)]` | 1 | 0 |

`match` の網羅性検査は保ったままで、`controller.rs` は production の
`clippy::cognitive_complexity` 警告から外れた。

## 方針の補正

当初案の「`AppEvent` / `AppKey` を context 別 enum にネストする」は採らなかった。`AppEvent` は
presentation・合成ルート・テストの広範囲から構築される公開語彙で、ネスト化は全構築サイトの書き換えを伴う。
求めていたのは「reducer の入口が全 context を 1 つの match で抱えないこと」であり、
委譲 arm + sub-reducer + 名前付き handler で同じ結果が得られるため、そちらを採った。

## 残件

`controller/tests.rs`（8,794 行）の context 別分割は #762（テスト配置）で扱う。

## 完了条件（達成状況）

- [x] `update_event` が 100 行以下、`cognitive_complexity` 25 以下
- [ ] `controller/tests.rs` の分割 → #762 へ移管
