---
number: 418
title: fix(tui): CreateSession の port ガード欠如と completion の種別不分別（create の無音喪失・エラー誤帰属）を直す
status: todo
priority: medium
labels: [fix, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:56:40.391129+00:00
updated_at: 2026-07-20T11:56:40.391129+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/presentation/mod.rs:1922` — `Effect::CreateSession` は無条件に `ui.creating_session = Some(...)` を立てる。一方 `Effect::RemoveSession`（:1933-1946）は `&& ui.session_commands.is_some()`（:1935）でガードされている。
- `drain_session_completions`（fn ~:1252-1284）は `let creating = ui.creating_session.take();`（:1255）で**コマンド種別を区別せず** take し、エラー時は `creating` が Some なら create ダイアログの `OperationResult`、None なら `BackendEvent::Notice` に振り分ける。

## 問題

- port 未注入時に CreateSession が pending 表示のまま完了しない（無音喪失）。
- remove 実行中に create を発行すると、remove の失敗が create 失敗ダイアログへ誤帰属するか、create の completion が黙って消える（競合時の取り違え）。

## 改善案（要検討）

- CreateSession にも port ガードを追加し、未注入時は即エラー通知にする。
- completion にコマンド種別（create/remove/refresh）を持たせ、`creating_session` の take を対応する種別の completion に限定する。

## 受け入れ条件

- [ ] port 未注入時の create が無音で消えない（明示エラー）。
- [ ] create と remove の並行時にエラーが正しい側へ帰属することがテストで固定されている。
- [ ] coverage 100% を維持する。
