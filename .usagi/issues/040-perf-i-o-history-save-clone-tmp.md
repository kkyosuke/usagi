---
number: 40
title: perf: 永続化 I/O と検索の最適化（history 追記・save clone・tmp 名統一）
status: todo
priority: medium
labels: [perf, core]
dependson: []
related: []
created_at: 2026-06-17T22:50:58.301215+00:00
updated_at: 2026-06-17T22:50:58.301215+00:00
---

## 背景

ストア・永続化まわりの中粒度のパフォーマンス問題をまとめて扱う。

### 1. history append が毎回全履歴を read→deserialize→serialize→write（`src/infrastructure/history_store.rs:61-65`）
home でコマンド実行のたびに全履歴をフル読み書きするため、履歴が伸びると 1 コマンドあたりのコストが線形に増え全体で O(N²)。
→ JSONL 化して `OpenOptions::append` で 1 行追記（`error_log.rs:63` が既にこのパターン）。または上限件数でリングバッファ化。

### 2. 検索が全本文を毎クエリ `to_lowercase` で確保（`src/usecase/issue/mod.rs:155-177`、`src/usecase/memory/mod.rs:102-120`）
`search` は必ず `scan()`（全 md パース）した上で title/body を毎回 `to_lowercase()`。body が大きいほどアロケーション増。query 空でも全件 scan。
→ query 空時は `summaries()`（index）経路へフォールバック。`needle.is_empty()` 判定をループ外へ。

### 3. save 時の全件 clone（`storage.rs:83,100`、`workspace_store.rs:78`、`history_store.rs:74`）
serialize するだけなのに version 付与のため所有データを丸ごと clone している。
→ 借用版の Serialize ラッパ（`workspaces: &'a [Workspace]` 等）で clone を避ける。

### 4. 固定 `.tmp` 名による並行 write 衝突リスク（`issue_store.rs:225`、`memory_store.rs:221`）
`write_atomically` が固定 `.tmp` 名を使うため、MCP とエージェントフックが同一ディレクトリへ並行 write すると temp ファイルを互いに上書きしうる。`json_file.rs` は pid+カウンタでユニーク化済み。
→ 3 箇所の重複実装を 1 つに集約し pid+カウンタ命名へ統一。

### 5. session create で `source_repos()` 再帰走査が複数回（`src/usecase/session/reconcile.rs:37`、`src/usecase/session/mod.rs:79`）
`create` 内で `reconcile` とブランチ競合チェックループが独立に再帰 `read_dir` 走査。
→ 結果を 1 回計算して引き回す。

## 確認方法

- 各操作の I/O が件数に対し線形以下になること。
- 並行 write での temp ファイル衝突が起きないこと。
- 既存テストが通ること（カバレッジ 100% 維持）。
