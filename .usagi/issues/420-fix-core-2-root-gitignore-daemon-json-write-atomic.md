---
number: 420
title: fix(core): 残存する非アトミック書き込み 2 箇所（ユーザー root .gitignore / daemon.json）を write_atomic 化する
status: todo
priority: medium
labels: [fix, core, infra, review]
dependson: []
related: []
created_at: 2026-07-20T11:57:10.703188+00:00
updated_at: 2026-07-20T11:57:10.703188+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。ストア類はアトミック置換（`json_file::write_atomic` 系）に統一済みだが、2 箇所だけ素の `fs::write` が残っている。

## 根拠（検証済み）

- `crates/core/src/infrastructure/gitignore.rs:58` — ユーザーのリポジトリ root の `.gitignore` を `fs::write(&gitignore, output)` で素書き（非アトミック）。
- `src/runtime/daemon.rs:1611-1615` — `FsRecordFile::write` が daemon.json を `std::fs::write(&self.path, contents)` で素書き。daemon.json が半端に壊れるとクライアント側の record load が InvalidData で失敗し、**全クライアントが daemon へ接続不能**になる。
- アトミックヘルパは既存: `crates/core/src/infrastructure/persistence/json_file.rs` の `write_atomic` / `write_text_atomic` / `write_atomic_cache`（markdown_store.rs:175 等で使用中）。

## 問題

プロセス kill・電源断・ディスクフルのタイミングで、ユーザーの `.gitignore` または daemon.json が破損する。daemon.json 破損は接続系の全断につながる。

## 改善案（要検討）

- 両者を `write_text_atomic` / `write_atomic` へ差し替える。
- `RecordFile` trait の doc に「実装はアトミック置換であること」を契約として明記する。

## 受け入れ条件

- [ ] 2 箇所が temp+rename のアトミック置換で書かれる。
- [ ] `RecordFile` の契約が doc 化されている。
- [ ] coverage 100% を維持する。
