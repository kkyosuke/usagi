---
number: 132
title: refactor(test): with_data_dir テストヘルパを test_support に集約する
status: done
priority: low
labels: [refactor, infra, review]
dependson: []
related: []
created_at: 2026-07-04T23:17:44.461087+00:00
updated_at: 2026-07-04T23:17:44.461087+00:00
---

## 背景（なぜ問題か）

`$USAGI_HOME` を tempdir に差し替えて body を実行し戻す `with_data_dir` ヘルパが、`agent_state_store` / `agent_prompt_store` / `agent_live_prompt_store`（`FnOnce(&Path)` 版）と `pr_link_store` / `open_panes_store` / `resume_focus_store` / `unite_store`（`FnOnce()` 版）の **7 ファイル**にコピペされている（2 シグネチャ）。`process_env_guard` は既に `test_support` に集約済みなので、同様に寄せられる。

## 対象箇所

上記 7 ストアの `#[cfg(test)] mod tests` 内の `with_data_dir`、`src/test_support.rs`

## やること

- 2 シグネチャを統一して `crate::test_support` へ 1 本化する。

## 受け入れ条件

- 各テストが共有ヘルパを使い、重複が消える。
- 既存テストが緑、カバレッジ 100% 維持。
