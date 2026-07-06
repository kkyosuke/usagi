---
number: 117
title: refactor(infra): issue_store/memory_store の markdown+index 永続化を MarkdownStore に共通化する（memory の鮮度判定欠落も是正）
status: done
priority: high
labels: [refactor, infra, review]
dependson: []
related: [116]
parent: 113
created_at: 2026-07-04T23:13:59.167071+00:00
updated_at: 2026-07-06T03:21:50.189247+00:00
---

## 背景（なぜ問題か）

`infrastructure/issue_store.rs`（実質 ~453 行）と `infrastructure/memory_store.rs`（実質 ~388 行）は「markdown ファイル群を SoT、`index.json` を派生キャッシュ」という永続化をほぼ同型で二重実装している。`IndexFile`/`IndexFileRef`、`scan`/`scan_lenient`（rayon 並列 + ErrorLog スキップ）、`*_files`、`read`、`write`/`write_locked`、`reindex_after_write`/`reindex_after_remove`（binary_search で splice）、`write_index`/`write_derived`、`remove`、`summaries`、`load_index`（corrupt 時 ErrorLog 記録 + None）、`rebuild_*` が、キー（u32 番号 vs name）と TOC（MEMORY.md）有無を除いてコピペになっている。

さらに **memory 側には issue 側の `load_fresh_index`（mtime/件数によるキャッシュ鮮度判定）が無く**、外部編集後に memory `summaries` が stale キャッシュを返す潜在的不整合がある（issue はこれを防いでいる）。共通化すれば挙動も揃う。

## 対象箇所

- `src/infrastructure/issue_store.rs`
- `src/infrastructure/memory_store.rs`

（`markdown_file.rs` はプレビュー読み取り専用で無関係）

## やること

- エントリのキー導出・summary 生成・TOC 描画だけを型パラメータ/トレイト（`MarkdownEntry` 等）で差し替える `MarkdownStore<E>` を新設し、両ストアをその薄いラッパにする。
- 鮮度判定 `load_fresh_index` は共通側に寄せ、memory にも適用する。

## 受け入れ条件

- 両ストアの公開 API と既存テストが不変のまま、共通ロジックが 1 か所に集約される。
- memory も外部編集後に stale を返さない（鮮度テストを追加）。カバレッジ 100% 維持。

## 補足

親 #113。usecase CRUD 共通化（#116）と設計を揃えると全体最適。infra 単独でも着手可能なため #116 とは related（ブロックしない）。
