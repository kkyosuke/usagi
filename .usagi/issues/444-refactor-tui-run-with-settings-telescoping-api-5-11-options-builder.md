---
number: 444
title: refactor(tui): run_with_settings_* の telescoping API（5 段委譲・11 引数）を options/builder に整理する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:03:03.607268+00:00
updated_at: 2026-07-20T12:03:03.607268+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/tui/src/presentation/mod.rs` のエントリポイント群が telescoping（引数追加のたびに関数名が伸びる）になっている:

- `run_with_settings`（:2457）→ `..._agent_port_factory`（:2489）→ `..._and_model_availability`（:2521）→ `run_with_settings_and_agent_and_metrics_port_factory_and_model_availability`（:2556-2581、**引数 11 個**: term, workspaces, recent, now, start, loader, settings, session_commands, agent_commands, available_models, metrics）→ `run_with_settings_inner`（:2621）の 5 段委譲。

## 問題

port を 1 つ足すたびに新しい関数名と委譲段が増え、呼び出し側（合成ルート・テスト）の更新点が増殖する。引数の順序取り違えも起きやすい。

## 改善案（要検討）

- `RunOptions`（builder または Default 持ち struct）に ports・設定を集約し、公開エントリポイントを 1 つにする。
- 既存の呼び出し互換が必要な間は thin wrapper を残し、テスト移行後に削除する。

## 受け入れ条件

- [ ] 公開エントリポイントが options 型を受ける 1 関数に集約されている。
- [ ] 既存挙動が回帰しない（既存テスト維持）。coverage 100% を維持。
