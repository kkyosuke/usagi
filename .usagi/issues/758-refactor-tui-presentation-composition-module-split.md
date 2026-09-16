---
number: 758
title: refactor(tui): presentation/mod.rs を bounded context 別 module に分割し frame loop の複雑度を下げる
status: todo
priority: high
labels: [v2, refactor, tui]
dependson: []
related: [757, 759, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`crates/tui/src/presentation/mod.rs` は 9,906 行の production コード（`mod tests` は別ファイル）に top-level 関数 160、
型 73、`impl` 42 を抱え、直近 400 commit で 151 回変更されている。`#[coverage(off)]` は 2 件だけなので、残る 158 関数は
テスト対象のロジックであり、architecture test が「composition module」と呼ぶ場所に次の bounded context が同居している。

| 領域 | 関数例 |
|---|---|
| 入力 routing | `route_workspace_input_before_reducer` / `route_garden_input` / `handle_work_run_control_input*` / `retarget_drawer_chords` |
| session command | `begin_session_command` / `apply_session_projection` / `drain_session_completions` / `emit_session_command_result` |
| terminal restore | `spawn_restore_job` / `pane_restore_targets` / `apply_restore_completion` / `restore_open_panes` |
| director | `select_director_agent` / `director_drawer_projection` / `open_director_from_new_button` |
| config / welcome flow | `step_config` / `step_workspace_config` / `run_config_save_loading` / `step_welcome` / `step_new` |

frame loop の `drive_workspace_controller` は 1,353 行で workspace 最長の関数であり、`clippy::cognitive_complexity` は 129
（既定閾値 25）を示す。`run_screen_graph_with_backend_and_notice`（382 行）、`drain_controller_host_actions`（244 行）、
`intercept_live_terminal_control`（223 行）も 200 行を超える。

## 方針

- 上表の領域ごとに `presentation/{input_routing,session_commands,restore,director,config_flow}.rs` へ切り出し、
  `mod.rs` には module 宣言と public entry だけを残す。
- `drive_workspace_controller` は `drain → poll → render → input → dispatch` の各段を `FrameLoop` の method に分け、
  各段を fake `Terminal` / fake port の unit test で固定する。`#[coverage(off)]` を残すのは実 `Terminal` を束ねる
  最外殻だけにする。
- `tests/architecture.rs` の `tui_presentation_keeps_tests_and_observation_policy_out_of_its_composition_module` を
  拡張し、切り出した領域が `mod.rs` に戻らないことを guard する。

## 完了条件

- `presentation/mod.rs` が 1,500 行以下、production 関数で 300 行を超えるものが無い。
- `cognitive_complexity` 100 超の関数が workspace に残らない。
- `document/03-tui.md` の該当節が分割後の module 名を指す。
