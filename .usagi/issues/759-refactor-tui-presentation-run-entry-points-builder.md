---
number: 759
title: refactor(tui): 9 本の run_* 入口と引数増殖を RunConfig builder に統合する
status: done
priority: medium
labels: [v2, refactor, tui]
dependson: [758]
related: []
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T16:17:07.619187+00:00
---

## 問題

`crates/tui/src/presentation/mod.rs` には TUI を起動する public 入口が 9 本あった。

- `run_workspace_controller_with_backend` / `_and_settings` / `_and_config`
- `run_workspace_deck_with_backend_and_config`
- `run_workspace_controller`
- `run_with_settings`
- `run_with_settings_and_agent_and_metrics_port_factory_and_model_availability`
- `run_screen_graph_with_backend` / `_and_notice`

名前の末尾に依存を足していく形で増えており、新しい port を 1 つ足すたびに入口が 1 本増えるか、既存入口の引数が伸びていた。

## やったこと

- `ScreenGraphRun` / `WorkspaceDeckRun` の 2 つの config struct（builder）を追加し、public 入口を
  `run_screen_graph` と `run_workspace_deck` の **2 本**にした。`notice` と `available_models` は
  `with_*` で渡す optional にしたので、`_and_notice` 系の分岐が要らなくなった。
- 合成ルート（`src/runtime/tui.rs`）の 5 箇所を新 API へ移行。
- 残る 7 本は crate 内テストからしか呼ばれていなかったので `#[cfg(test)] pub(crate)` にした。これに伴い
  production build で使われなくなった支援型（`FixedBackendFactory` / `CompatibilityBackendFactory` /
  `NoMetrics` / `Unavailable*Port` / `DefaultSettingsPort` ほか 27 件）と、その import も `#[cfg(test)]` へ落とし、
  出荷ビルドから dead code を外した。
- `run_workspace_deck_with_backend_and_config` は `run_workspace_deck` へ畳んで `#[allow(clippy::too_many_arguments)]`
  を 1 件返済した。

## 完了条件（達成状況）

- [x] `presentation/mod.rs` の `pub fn run_*` が 2 本
- [~] `#[allow(clippy::too_many_arguments)]` — public 入口からは無くなった。`mod.rs` に残る 8 件のうち 6 件は
  `#[cfg(test)]` のテスト用 wrapper、2 件は内部 frame helper（`open_snapshot_via_controller` /
  `enter_workspace_deck`）である。後者の棚卸しは #766 で扱う。
