---
number: 37
title: perf: issue/memory ストアの index をインクリメンタル更新にする
status: todo
priority: high
labels: [perf, core]
dependson: []
related: []
created_at: 2026-06-17T22:50:18.538509+00:00
updated_at: 2026-06-17T22:50:18.538509+00:00
---

## 背景

`IssueStore::write`/`remove`（`src/infrastructure/issue_store.rs:130-144,186-201`）と `MemoryStore::write`（`src/infrastructure/memory_store.rs:120-128,170-187`）は、1 件の更新ごとに `rebuild_index()` → `scan()` で**ディレクトリ内の全 `.md` を read + parse し直して** index.json を丸ごと再生成している。memory は index.json と MEMORY.md の 2 ファイルを毎回フル再生成。

1 件更新が O(全件) の I/O + パースを誘発するため、MCP 経由でエージェントが issue/memory を連続操作する usagi の主要ワークフローで **O(N²)** になる。

関連して以下も同根:
- `files_for`（`issue_store.rs:204-215`）が呼ばれるたびに `read_dir` 全走査。`write` パスでは `files_for` と `rebuild_index` 内 `scan` で重複列挙。
- `max_number()`（`issue_store.rs:101`）が全 summaries 経由。ファイル名プレフィックス（`NNN-`）の最大値で十分で md パース不要。

## 改善方針

- 既存 index をロードし、対象 1 件のサマリだけ差し替え/削除/挿入する差分更新にする。全 `scan` は index が壊れている/欠けているときのフォールバックに限定。
- `write` パスの `read_dir` 列挙結果を使い回す。
- `max_number` はファイル名から算出する。

## 確認方法

- issue/memory を連続 create/update したときの I/O が件数に対し線形（O(N)）になること。
- 既存テストが通ること（カバレッジ 100% 維持）。
