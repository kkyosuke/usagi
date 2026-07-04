---
number: 48
title: refactor(usecase): 設定セッターと永続化 version ラッパのボイラープレートを削減する
status: done
priority: low
labels: [refactor, infra]
dependson: []
related: []
created_at: 2026-06-18T22:41:44.308440+00:00
updated_at: 2026-07-04T00:15:28.307360+00:00
---

## 背景

設定更新と JSON 永続化に「同型の数行関数」が横展開されており、項目やファイルが増えるたびに増殖する。

### 1. 設定セッターが同型ボイラープレート（中）
`usecase/settings.rs` の `set_theme` / `set_default_workspace` / `set_notifications_enabled` / `set_agent_cli` は「`load_settings` → 1 フィールド代入 → `save_settings` → 返す」が完全に同型。ローカル 2 本（`set_local_agent_cli` / `set_local_notifications_enabled`）も同型。なお `set_default_workspace` は呼び出しが見当たらず、デッドコードの可能性。

### 2. 永続化の version ラッパが 4 重定義（低）
`{ version: u32, #[serde(flatten)] inner }` というラッパ構造体と `FILE_FORMAT_VERSION` 定数、および同形の `load`/`save` が `infrastructure/workspace_store.rs`（`StateFile`/`LocalSettingsFile`）と `infrastructure/storage.rs`（`WorkspacesFile`/`SettingsFile`）に重複定義されている。

## 改善方針

- `update_settings(storage, |s| { ... })` / `update_local(repo, |l| { ... })` の「ロード→クロージャで変更→保存→返す」高階関数 1 本に集約し、呼び出し側は `|s| s.theme = theme` のように書く。`set_default_workspace` は利用箇所を確認のうえ未使用なら削除する。
- `json_file` にジェネリックな versioned read/write（例 `read_versioned<T>` / `write_versioned<T>`）を用意し、各ストアはそれを呼ぶだけにする。最低でも version ラッパは 1 か所のヘルパに寄せる。

## 確認方法

- 設定の読み書き・グローバル/ローカルの解決が従来どおりであること（settings テスト）。
- 各永続化ファイルのフォーマット（version 付与）が変わらないこと（data ストアのテスト）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
