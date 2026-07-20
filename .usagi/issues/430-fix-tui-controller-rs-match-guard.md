---
number: 430
title: fix(tui): controller.rs の到達不能 match アーム（副作用持ち guard の後段）を削除する
status: todo
priority: medium
labels: [fix, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:59:30.373113+00:00
updated_at: 2026-07-20T11:59:30.373113+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。coverage(off) が gate をすり抜けさせた実害例なので独立 issue とする。

## 根拠（検証済み）

- `crates/tui/src/usecase/application/controller.rs:2169` — `AppEvent::Backend(event) if update_editor_backend(state, &event) => Vec::new()` の guard 付きアーム。
- `update_editor_backend`（:2310-2392）は該当 8 variant（NotesLoaded/NotesError/EnvironmentLoaded/EnvironmentError/PullRequestsLoaded/PullRequestsError/PreviewLoaded/PreviewError）すべてで `true` を返す（`_ => return false` ＋末尾 `true`、:2390-2391）。
- したがって同じ 8 variant を列挙する後段のアーム（:2212-2221）は**到達不能**。coverage(off) により未実行のまま gate を通過して残存している。
- さらに guard の `update_editor_backend` は `editor.scratchpad`／`editor.error`／overlay フィールドを**書き換える副作用**を持つ（match guard としては危険なパターン）。

## 問題

到達不能コードが「本番挙動」に見える形で残り、読み手が二重処理と誤認する。副作用持ち guard は評価順に依存し、アーム追加・並べ替えで挙動が変わる罠になる。

## 改善案（要検討）

- 到達不能アーム（:2212-2221）を削除する。
- guard を「判定のみ」にし、状態変更は match 本体へ移す（`update_editor_backend` を bool 判定と apply に分離）。

## 受け入れ条件

- [ ] 到達不能アームが存在しない（clippy/コンパイラ警告または reachability を示すテストで確認）。
- [ ] guard が副作用を持たない。
- [ ] 既存の editor backend イベント処理の挙動が回帰しない。coverage 100% を維持。
