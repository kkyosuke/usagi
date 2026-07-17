---
number: 312
title: fix(tui): workspace state read errors are surfaced
status: done
priority: high
labels: [tui, bug, state]
dependson: []
related: [301]
created_at: 2026-07-17T11:11:05.387614+00:00
updated_at: 2026-07-17T12:03:30.202011+00:00
---

## 目的

Workspace を開くとき、repository-local `.usagi/state.json` が壊れている・読めない場合に空状態として続行せず、安全な UI エラーとして返す。未作成の state だけは既定の空 state として扱う。

## 受け入れ条件

- `WorkspaceStateStore::load` の `Ok(None)` は空の `WorkspaceState` に変換する。
- read・parse・permission などの `Err` は `FsWorkspaceLoader::open` から `io::Error` として上位へ伝搬し、Workspace UI を起動しない。
- 壊れた state と未作成 state の回帰テストで挙動を固定する。
- TUI の起動時 error 表示の既存 UX を維持する。
- 実装仕様ドキュメントを更新する。
