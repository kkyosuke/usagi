---
number: 759
title: refactor(tui): 9 本の run_* 入口と引数増殖を RunConfig builder に統合する
status: todo
priority: medium
labels: [v2, refactor, tui]
dependson: [758]
related: []
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`crates/tui/src/presentation/mod.rs` には TUI を起動する public 入口が 9 本ある。

- `run_workspace_controller_with_backend` / `_and_settings` / `_and_config`
- `run_workspace_deck_with_backend_and_config`
- `run_workspace_controller`
- `run_with_settings`
- `run_with_settings_and_agent_and_metrics_port_factory_and_model_availability`
- `run_screen_graph_with_backend` / `_and_notice`

名前の末尾に依存を追加していく形で増えており、`#[allow(clippy::too_many_arguments)]` 27 件のうち 14 件がこのファイルの
入口・frame 関数に付く。新しい port を 1 つ足すたびに入口が 1 本増えるか、既存入口の引数が伸びる。

## 方針

- 注入する port / settings / notice / model availability を 1 つの `RunConfig`（builder）にまとめ、public 入口を
  「production 用」「screen graph harness 用」の 2 本に減らす。
- 既存の呼び手（合成ルート `src/runtime/tui.rs`、`tests/`、`examples/`）を builder へ置き換える。
- `too_many_arguments` の allow を入口から外す。

## 完了条件

- `presentation/mod.rs` の `pub fn run_*` が 2 本以下。
- `presentation/mod.rs` に `#[allow(clippy::too_many_arguments)]` が残らない。
