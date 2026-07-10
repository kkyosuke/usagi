---
number: 161
title: feat(daemon): セッション監視ティック（Step 2 集約エンジン）
status: done
priority: medium
labels: [daemon, cli]
dependson: []
related: []
parent: 159
created_at: 2026-07-10T00:16:54.472086+00:00
updated_at: 2026-07-10T00:16:59.374014+00:00
---

Epic #159 の Step 2。session monitor の phase 由来部分を daemon へ移す（**通知は出さない集約エンジン**に限定）。

## 実装内容

- daemon の serve ループが毎ティック、全登録ワークスペースのセッションを走査し、各セッションの `SessionActivity`（agent phase 由来: ready/running/waiting/done）を集約したスナップショットを `<data-dir>/daemon/sessions.json` に保存（変化時のみ書き込み）。
- `usagi daemon status` が running のとき、監視中のセッション一覧（名前＋activity）を表示。

## スコープ判断（重要）

- **通知は発火しない**。bell 信号と通知の一次発火は TUI 所有の PTY（Step 3 まで）に依存し、今 daemon から通知すると TUI 併用時に**二重通知**になる。通知調停は Step 4。
- **daemon→TUI の push は Step 3 の IPC socket と一緒**に入れる。現状は TUI が引き続き自前の state.json watcher を使う。本 Step は「daemon が Sessions ビューを構築・保存する」エンジン部分。

## 層構成

- `domain/daemon.rs` — `SessionActivity`（phase→activity 純粋マップ）+ `SessionSnapshot`。
- `infrastructure/daemon_sessions_store.rs` — スナップショットの read/write（`sessions.json`）。
- `usecase/daemon.rs` — `gather`（roots/sessions/phase を注入して集約）+ `monitor_tick`（差分時のみ永続化）。
- `presentation/cli/daemon.rs` — `status` のセッション一覧表示。
- `src/main.rs`（合成ルート・除外）— serve ループへの組み込みと実ストア読み取りアダプタ。

## テスト

- domain/usecase/infra/presentation の全分岐をユニットテストでカバー（カバレッジ 100%: lines/functions）。
- `tests/daemon_monitor_test.rs` — 実 `WorkspaceStore` + `agent_state_store` を経由した `gather` の統合テスト（worktree→phase→activity のマッピングを authentic に検証）。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md)
