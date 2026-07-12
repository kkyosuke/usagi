---
number: 211
title: feat(core): issue / memory / workspace の永続化ストアを usagi-core へ移行する
status: done
priority: high
labels: [core, infra]
dependson: []
related: []
created_at: 2026-07-12T02:11:26.933663+00:00
updated_at: 2026-07-12T02:11:31.984480+00:00
---

## 目的

#210 で移行した domain エンティティ（issue / memory / workspace）を実際にディスクへ保存・読込する
infrastructure 永続化層を v1 から usagi-core へ移行する。ユーザー指定により **3 エンティティすべて**を
**v1 完全移植**（ロック・並列スキャン・error_log・index freshness まで）で行う。

## 対象（v1 → v2 `crates/core/src/infrastructure/`）

| v1 | v2 | 役割 |
|---|---|---|
| `repo_paths.rs` | `repo_paths.rs` | `<repo>/.usagi` の配置 |
| `json_file.rs` | `json_file.rs` | アトミック書き込み・versioned JSON envelope |
| `store_lock.rs` | `store_lock.rs` | cross-process 排他ロック（fs2） |
| `error_log.rs` | `error_log.rs` | 日次ローテーションのエラーログ（`ErrorLog` のみ。Logger 系は TUI 専用のため後続） |
| `markdown_store.rs` | `markdown_store.rs` | frontmatter markdown ＋ 派生 `index.json` の汎用ストア（`FrontmatterDoc` の初の利用者） |
| `issue_store.rs` | `issue_store.rs` | issue の CRUD・採番・index |
| `memory_store.rs` | `memory_store.rs` | memory の CRUD・`MEMORY.md` TOC・index |
| `storage.rs`（workspaces 部分） | `storage.rs` | 既定データディレクトリ解決＋`workspaces.json` レジストリ |

## スコープ判断

- `storage.rs` / `workspace_store.rs`（v1）は未移行の domain（`Settings` / `WorkspaceState`）に依存するため、
  移行済みの `Workspace` レジストリ（`workspaces.json`）に絞って移植する。`Settings` と repo `state.json` は後続。
- 追加依存: `serde_json`（本依存へ昇格）・`anyhow`・`fs2`・`dirs`・`rayon`、dev の `tempfile`。
- v2 の `clippy::pedantic`（`-D warnings`）と edition 2024（`std::env::set_var` の `unsafe` 化）に合わせて調整。

## 完了条件

- fmt / clippy(pedantic) / full test / coverage 100%（lines・functions）/ Markdown link check が通る。
- `cargo run` が従来どおり動く。
