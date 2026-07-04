---
number: 133
title: refactor(cli): config.rs の enum ラベル関数を domain の as_str/serde と SSoT 化する
status: todo
priority: low
labels: [refactor, cli, review]
dependson: []
related: [115]
created_at: 2026-07-04T23:17:55.759631+00:00
updated_at: 2026-07-04T23:17:55.759631+00:00
---

## 背景（なぜ問題か）

`presentation/cli/config.rs` の `theme_label` / `agent_label` / `session_action_ui_label` / `sidebar_label` / `key_scheme_label` は「on-disk ラベル」（docstring に明記）を手書き match で再エンコードしているが、その文字列は domain 側 enum の `#[serde(rename_all="snake_case")]` が既に定義している。variant 追加/改名時に二重メンテになり drift しうる。

## 対象箇所

- `src/presentation/cli/config.rs` の `*_label` 群
- `src/domain/settings.rs` の各 enum

## やること

- domain 側に `as_str`（既に一部 enum に存在）を用意し serde と単一定義にする。
- CLI 表示は共有 `as_str` 経由にする。

## 受け入れ条件

- on-disk ラベルの定義が 1 箇所になり、`render_settings` 出力が不変（テストで固定）。
- 既存テストが緑、カバレッジ 100% 維持。

## 補足

enum の `as_str`/Display/FromStr トリオ集約 #115 と同じ「enum 文字列 SSoT」テーマのため related。#115 が domain の enum 側を整えると、この CLI 側の再エンコード撤去が容易になる。
