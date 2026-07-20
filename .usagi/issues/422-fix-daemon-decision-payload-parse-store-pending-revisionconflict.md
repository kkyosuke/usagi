---
number: 422
title: fix(daemon): decision payload の parse 失敗・store 失敗・非 pending がすべて RevisionConflict に潰される分類を分離する
status: todo
priority: medium
labels: [fix, daemon, review]
dependson: []
related: []
created_at: 2026-07-20T11:57:28.364326+00:00
updated_at: 2026-07-20T11:57:28.364326+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `src/runtime/daemon.rs:1260-1306`（`dispatch_user_decision` 内）— payload の parse 失敗が軒並み `.map_err(|_| UserDecisionError::Terminal)`（:1263, :1274, :1282, :1288, :1294）。store 失敗も同じ `Terminal` へ。非 pending 状態も同様。
- :1303 で `Terminal` は一律 `(ErrorCode::RevisionConflict, "decision is not pending or is outside this workspace")` に写像される。

## 問題

呼び出し側（MCP agent / TUI）から見ると、「payload が壊れている」「store がエラー」「decision が既に解決済み」がすべて同じ RevisionConflict になる。リトライすべきか、payload を直すべきか、諦めるべきかを判断できず、デバッグも困難。

## 改善案（要検討）

- parse 失敗 → `InvalidArgument`、store 失敗 → `Unavailable`（または Internal）、非 pending → `RevisionConflict` に分離する。
- エラーメッセージに失敗した field 名等の文脈を含める。
- 関連: `dispatch_user_decision` 一式の `usagi-daemon::presentation::ipc` への移設 issue（そちらが先行する場合は移設後のコードに適用）。

## 受け入れ条件

- [ ] 3 種の失敗が別々の ErrorCode で返ることがテストで固定されている。
- [ ] coverage 100% を維持する。
