---
number: 442
title: refactor(daemon): handle_connection の 4 変種を 1 本化し、残存する未知 kind の Ok+echo フォールバックを typed エラーへ変える
status: todo
priority: medium
labels: [refactor, daemon, review]
dependson: []
related: [401]
created_at: 2026-07-20T12:02:46.827723+00:00
updated_at: 2026-07-20T12:02:46.827723+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。#401（false-success no-op のエラー化、マージ済み）は `dispatch_tool`/`supervisor_tool` の 2 系統を明示エラー化したが、**それ以外の未知 kind は依然 Ok+echo に落ちる**。

## 根拠（検証済み）

- `crates/daemon/src/presentation/ipc.rs` に handle_connection 変種が 4 つ: `handle_connection`（:125）・`handle_connection_with`（:137）・`handle_connection_with_terminal`（:190）・`handle_connection_with_terminal_and`（:263）— ほぼ同一のループ・エラー処理を段階的引数違いで重複。
- `dispatch()`（:84-120）: `dispatch_tool`/`supervisor_tool` は #401 の修正で `ErrorCode::InvalidArgument`（:90-97、regression test :683）。しかし **else 分岐（:99-110）は `{session,agent,dispatch}` 以外の未知 kind に `ResponseOutcome::Ok` ＋ body エコー**を返す。`kind_response()`（:347）も非 Response envelope に `Ok + json!(null)`。

## 問題

- 4 変種の重複は変更漏れの温床（1 つ直して 3 つ直し忘れる）。
- 未知 kind への Ok+echo は「タイポした kind のリクエストが成功に見える」false-success の残り火で、#401 の趣旨と不整合。

## 改善案（要検討）

- 変種を builder/options 引数の 1 実装に統合する。
- 未知 kind は typed `InvalidArgument`（"unknown request kind: <kind>"）を返す。既知 3 kind の挙動は不変。
- 関連: 合成ルートの kind ルーティング移設 #432（同じファイルに手が入るため実施順を調整）。

## 受け入れ条件

- [ ] handle_connection が 1 実装になり、既存呼び出しがすべてそこを通る。
- [ ] 未知 kind が Ok+echo でなく InvalidArgument になることがテストで固定されている。
- [ ] coverage 100% を維持する。
