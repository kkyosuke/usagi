---
number: 406
title: usagi mcp: user_decision の last-mile を接続する（TUI credential＋解決回答の caller 配送）
status: in-progress
priority: medium
labels: [mcp]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:54:50.970575+00:00
updated_at: 2026-07-20T08:51:39.157767+00:00
---

親: #400。依存: #401。`user_decision_*` は store まで到達する部分実装だが、(1) 人間（TUI）が回答できない、(2) 回答が元 agent へ返らない、の 2 つの last-mile が断たれている。これを繋ぐ。

## 対象 tool（6）

`user_decision_request` / `user_decision_get` / `user_decision_list` / `user_decision_resolve` / `user_decision_cancel` / `user_decision_expire`。

## 現状の断絶（根拠）

- credential 付き agent → `dispatch_user_decision`（`src/runtime/daemon.rs:1122`）→ 実 `UserDecisionStore` までは到達する。
- **TUI 側**: `src/runtime/tui.rs:130-135`・`:162-167` が `caller_context: None` を送るため、`daemon.rs:1208-1213` で `OwnershipUnknown` fail-closed。人間が pending decision を list/resolve できない。
- **caller 返却**: 解決回答は outbox（`crates/core/src/infrastructure/store/user_decision.rs:111-116`）へ積まれるが、`.events()` の consumer が production に存在しない（test のみ）。元 agent への配送経路が無い（module doc `:4-5` も「aspirational」と明記）。

## 完了条件

- [ ] **TUI 回答経路**: 人間が pending user decision を list/get/resolve/cancel できる。TUI（人間）は agent の credential を持たないため、`dispatch_user_decision` の owner 解決を「credential 経路（agent 起票）」と「workspace-scoped な TUI 回答経路」に分け、TUI からの list/resolve が正しく認可される設計にする（credential fail-closed を人間回答に流用しない）。
- [ ] **caller 配送（last-mile）**: 解決した `UserDecisionResolvedEvent` を consumer が読み、元 caller agent へ届ける（例: `AgentInbox` へブリッジ、または blocked agent が `user_decision_get` でポーリング取得できることを保証）。outbox が未読で滞留しないこと。
- [ ] request/get/list/resolve/cancel/expire が durable 状態を反映した結果を返す（既存の credential 経路の provenance 検証を回帰させない）。
- [ ] **production E2E**: agent（credential 経路）が `user_decision_request`→（TUI 相当の回答経路で）`user_decision_resolve`→元 caller が回答を受領、までを stdio/IPC→実 daemon→durable で固定。
- [ ] docs（`07-mcp.md:26-29` の credential 記述、必要なら TUI 回答経路の追記）。coverage 100%。

## 関連

`#379`（TUI pending user-decision modal）・`#378`（daemon user_decision dispatch tool）・`#391`（user decision 通知）と重複しないよう突き合わせること。
