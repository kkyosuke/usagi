---
number: 764
title: refactor(tui): 実 IO を持たない Production*Port adapter を合成ルートから tui の infrastructure 層へ移す
status: todo
priority: medium
labels: [v2, refactor, tui, root]
dependson: []
related: [757, 759]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`crates/tui/src/infrastructure/mod.rs` は module doc 5 行だけで中身が無い。一方 `src/runtime/tui.rs`（9,899 行、
production 約 5,200 行）には `Production*Port` / `Daemon*Port` / `Fs*` / `Platform*` の adapter 型が 31、trait impl が 26
あり、`src/runtime/agent_tab_intent.rs`（1,598 行）は Agent tab intent の file store adapter そのものである。

これらの多くは daemon IPC client（`usagi_core::infrastructure::client`）や `std::fs` の上で TUI port を実装しており、
crossterm や実端末に依存しない。にもかかわらず合成ルートに置かれることで、tui crate 単体では「port の production 実装」
をテストできず、合成ルートの `#[coverage(off)]` 候補が増え続ける。

## 方針

- crossterm / 実 PTY / OS 通知 / clipboard など実 IO を伴う adapter だけを合成ルートに残し、daemon IPC client 上の
  adapter（`DaemonSessionCommandPort`、`DaemonMetricsPort`、`DaemonGardenInventoryPort` …）と file store
  （`FileAgentTabIntentStore`、`RepoEnvironmentStore`、`FsWorkspaceLoader`）を `crates/tui/src/infrastructure/` へ移す。
- 移した adapter は fake transport / tempdir で unit test し、`document/02-architecture.md` の依存ルール
  （tui は crossterm に依存しない）は維持する。
- `tests/architecture.rs` で「tui infrastructure は crossterm / portable-pty を import しない」を guard する。

## 完了条件

- `src/runtime/tui.rs` が 2,000 行以下。
- `crates/tui/src/infrastructure/` に adapter と unit test が置かれ、`coverage-off-budget.json` の `root-cli` 件数が減る。
