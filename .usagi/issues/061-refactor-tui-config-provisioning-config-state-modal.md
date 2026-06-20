---
number: 61
title: refactor(tui-config): provisioning ロジックの退避と config/state modal 分離
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-06-19T22:16:55.778677+00:00
updated_at: 2026-06-19T22:16:55.778677+00:00
---

## 背景

config 画面に責務過多・肥大が見られる。

### 1. provisioning ロジックが launcher に同居（`src/presentation/tui/config/mod.rs:127-181`）
本来「画面を起動して closure を注入する」だけであるべき `mod.rs` に、`start_install_runtime` / `start_pull_model` / `install_error_message`（バックグラウンドスレッド生成・`install_task` 進捗記録・`SetupError`→日本語メッセージ変換）が 55 行同居している。→ `usecase::local_llm` 側か専用モジュールへ移し、`mod.rs` は呼ぶだけにする。

### 2. config/state/mod.rs（862 行）の modal 分離
`cycling.rs` は分離済み。残りの肥大要因は install/model modal の委譲メソッド群（`:494-548`）と `InstallModal`/`ModelModal`/`ModelRow` 型定義。→ `state/modal.rs` へ括り出すと親 mod.rs は「Config 本体 + Field/LocalField + rows()/value_of()」に絞れ 300 行台に収まる。

### 3. config/event.rs（994 行）のテスト外出し
実コードは ~283 行で、肥大の主因は 711 行のテスト。→ `event/tests.rs` のように別ファイル化（state/ が既に `mod tests;` 外出しを採用しており前例あり）。

## 確認方法

- `mod.rs` が launcher 責務に純化すること。
- 既存テストが通ること（カバレッジ 100% 維持）。
