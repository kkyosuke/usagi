---
number: 432
title: refactor(daemon): 合成ルート daemon.rs の kind ルーティングと dispatch_* 群を usagi-daemon::presentation::ipc へ移設する（file-wide coverage(off) の解消込み）
status: todo
priority: high
labels: [refactor, daemon, review]
dependson: []
related: [422]
created_at: 2026-07-20T12:00:09.931074+00:00
updated_at: 2026-07-20T12:00:09.931074+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

合成ルート（ルート `src/`）は「store・PTY・環境変数の注入だけ」を持つ約束だが、`src/runtime/daemon.rs`（2,373 行）に IPC リクエストのルーティング・応答整形・認可判断がベタ書きされている。しかもファイル先頭の `#![coverage(off)]`（:3）により、この**純ロジック群が丸ごと coverage gate の外**にある。

## 根拠（検証済み）

- `src/runtime/daemon.rs:3` — ファイル全体 `#![coverage(off)]`。
- kind ルーティング :1046-1057 — `Some("session") => dispatch_session(...)` / `Some("dispatch") => dispatch_dispatch(...)` / `Some("dispatch_tool") => dispatch_user_decision(...)` / `_ => usagi_daemon::presentation::ipc::dispatch(...)`。
- `dispatch_user_decision` :1122-1317（認可判断 ~195 行）、`dispatch_dispatch` :1319-1392、`dispatch_session` :1435-1521、`available_worktree` :369-383 — いずれも純ロジック中心。

## 問題

- 責務境界: ルーティング・応答整形は `usagi-daemon::presentation::ipc` の責務であり、合成ルートに置くと daemon クレートのテスト資産から漏れる。
- coverage: 認可判断（user decision の owner 解決・fail-closed）というセキュリティ上重要な分岐が計測されていない。

## 改善案（要検討）

- kind ルーティングと `dispatch_user_decision` / `dispatch_dispatch` / `dispatch_session` / `available_worktree` を `crates/daemon/src/presentation/ipc.rs`（または新モジュール）へ移設し、store 等は port として注入する。
- 合成ルートには実 IO（socket accept・プロセス・環境変数）の注入だけを残し、`#![coverage(off)]` はその残余にのみ適用する。
- 関連: decision エラー分類の分離（#422。移設後コードに適用してよい）。

## 受け入れ条件

- [ ] ルーティング・dispatch_* 群が daemon クレート側に移り、ユニットテストで直接覆われている。
- [ ] `src/runtime/daemon.rs` は注入と実 IO のみ（行数が大幅減）。
- [ ] coverage 100% を維持し、移設分は計測対象になっている。
