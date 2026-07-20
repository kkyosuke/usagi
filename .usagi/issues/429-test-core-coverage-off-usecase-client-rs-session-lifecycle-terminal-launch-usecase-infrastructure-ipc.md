---
number: 429
title: test(core): coverage(off) の棚卸し（usecase/client.rs・session_lifecycle・terminal_launch・usecase 各所・infrastructure/ipc）
status: todo
priority: high
labels: [test, core, review]
dependson: []
related: []
created_at: 2026-07-20T11:59:19.547548+00:00
updated_at: 2026-07-20T11:59:19.547548+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

06-conventions は `#[coverage(off)]` を「実 IO そのもの」か「generic 単相化の重複計上」に限定するが、usagi-core では純ロジックまで広範に除外されており、100% gate の実効性が下がっている（repo 全体で coverage(off) は 850 箇所）。

## 根拠（検証済み）

- `crates/core/src/usecase/client.rs:7` — **ファイル全体** `#![coverage(off)]`。純関数 `decode_pr_snapshot`（:141）等も除外。
- `crates/core/src/domain/session_lifecycle.rs` — 純 reducer 本体に付与: `reduce`（attr :325）・`create_completed`（:422）・`fenced_session`（:450）・`complete`（:484）。
- `crates/core/src/domain/terminal_launch.rs:1` — ファイル全体 off（domain の純ロジック）。
- usecase 各所: `session.rs` 7/7 公開関数・`note.rs` 12 箇所・`issue.rs` 5/5・`workspace.rs` 8 箇所が off。
- `crates/core/src/infrastructure/ipc/mod.rs` — item 単位で 22 箇所。テスト済み純ロジック（`negotiate` :284、`ResponseCache` :360/:367/:383、frame codec :438-509、`OutboundQueues` :563-675、`IdempotencyLedger::decide` :419）まで除外。

## 問題

除外された純ロジックは回帰してもゲートに引っかからない。特に reducer（session_lifecycle）と IPC フレーム処理は correctness の中枢。

## 改善案（要検討）

- ファイル全体 off を剥がし、実 IO の薄い関数だけ item 単位で off に戻す。
- 既にテストが実行している純関数（ipc の codec 等）は即剥がせる。
- 各除外に 06-conventions 準拠の理由（実 IO / 単相化）をコメントで残す。
- 関連: IpcClient の infrastructure 移設 issue（client.rs はそちらの移設後に棚卸しすると差分が小さい）。

## 受け入れ条件

- [ ] 対象ファイルの coverage(off) が「実 IO そのもの」「単相化重複」のみになり、各除外に理由が記録されている。
- [ ] 剥がした箇所が coverage 100% を満たす（不足分はテスト追加）。
