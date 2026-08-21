---
number: 709
title: feat(daemon): tenant registry と workspace fence の多重保持で workspace を on-demand adopt する
status: todo
priority: high
labels: [v2, daemon, lifecycle, workspace]
dependson: [708]
related: []
created_at: 2026-08-20T23:33:39.823990+00:00
updated_at: 2026-08-21T00:10:11.622604+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 2。#708 が前提。

## 問題・根拠

daemon は `serve` の起動時に `FileWorkspaceFence` を 1 つだけ取得し、`SessionRuntime` を 1 インスタンスだけ持つ。
workspace root は起動時 cwd（durable な `repository_root` があればそちら）で 1 回確定する。

domain 層は既に workspace 次元を持つ（`TerminalRef.workspace_id`、`CompletionFence.workspace_id`、
terminal retention の workspace ごと usage と daemon 全体 aggregate、workspace 単位の `AgentInventory`）。
単数なのはこの保持側だけである。

## 方針

tenant（canonical root・`WorkspaceId`・保持中の fence guard・`SessionRuntime`）の registry を daemon に持たせる。

- 起動時 cwd の workspace を initial tenant として adopt する（現行の起動挙動と同じ）。
- adopt は canonical 化 → workspace fence 取得 → `SessionRuntime::open` → registry 登録の順（現行 `serve` と同じ取得順）。
- fence を他 process が持っていたら **その workspace だけ** typed refusal（owner pid を添える）。他 tenant は影響を受けない。
- adopt は 1 workspace につき直列化し、同時 adopt 数に上限を置く。
- shutdown は全 tenant を graceful に閉じ、fence を逆順に返す。custody 監視は既存の invariant を tenant ごとに評価する。

この段階では IPC からは initial tenant だけを使い、admission の変更は #710 で行う。

## 受入条件

- adopt / retire、fence 取得失敗がその workspace だけを拒否すること、同時 adopt の直列化が unit test で固定される。
- 2 つの fixture workspace を adopt した daemon で、片方の session 作成が他方の `sessions.json` を書かないことを結合テストで確認する。
- shutdown 後に両方の workspace fence が解放され、別 process が取得できる。
- カバレッジ 100% を維持する。
