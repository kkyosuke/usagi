---
number: 434
title: test(daemon): ファイル一括 coverage(off)（agent_ipc / session_runtime / generic_terminal / terminal_ipc / presentation::ipc、計 ~6,100 行）を棚卸しする
status: todo
priority: medium
labels: [test, daemon, review]
dependson: [411]
related: []
created_at: 2026-07-20T12:00:43.439570+00:00
updated_at: 2026-07-20T12:00:43.439570+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。06-conventions の除外規約（実 IO / 単相化のみ）に対し、daemon はファイル一括 off が常態化している。

## 根拠（検証済み）

ファイル全体 `#![coverage(off)]` の 5 ファイル（計 6,097 行）:

- `crates/daemon/src/usecase/agent_ipc.rs:20`（2,101 行）
- `crates/daemon/src/usecase/session_runtime.rs:8`（1,736 行）
- `crates/daemon/src/usecase/generic_terminal.rs:1`（797 行）
- `crates/daemon/src/presentation/ipc.rs:3`（742 行）
- `crates/daemon/src/usecase/terminal_ipc.rs:9`（721 行）

これらには idempotency ledger・エラー写像・JSON 変換・admission 判定など、実 IO を含まない業務ロジックが多数含まれる。

## 問題

daemon の中枢（IPC 受理・冪等・状態遷移）が回帰しても coverage gate に掛からない。到達不能アームの残存（tui 側 #430 と同型の問題）を検出できない。

## 改善案（要検討）

- ファイル一括 off を剥がし、実 IO（プロセス・PTY・socket）を触る薄い関数だけ item 単位で off にする。
- 未配線コードの削除（#411）の**後**に実施すると、剥がす対象が減って差分が小さい（本 issue は #411 に依存）。
- 各除外に規約準拠の理由を残す。

## 受け入れ条件

- [ ] 5 ファイルの coverage(off) が item 単位・理由付きになっている。
- [ ] 剥がした業務ロジックがテストで覆われ coverage 100% を満たす。
