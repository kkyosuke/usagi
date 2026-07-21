---
number: 496
title: refactor(v1): session inventory と setup runner を port で反転する
status: done
priority: medium
labels: [review, v1, architecture, session]
dependson: []
related: [59, 113]
parent: 453
created_at: 2026-07-20T12:07:07.703409+00:00
updated_at: 2026-07-21T13:54:50.912897+00:00
---

## 問題・影響

出荷中 v1 では `v1/src/infrastructure/agent_start_store.rs` が `usecase::session::{workspace_root,list}` を呼び、`v1/src/infrastructure/setup_runner.rs` が上層の `usecase::session::SetupCommandRunner` を import/実装する。infrastructure が session orchestration を知る逆依存で、adapter の独立テストと責務境界を壊す。

## 成立条件 / 再現フロー

module import graph で infrastructure→usecase edges を検出できる。session API/representation の変更が store/runner を直接巻き込み、composition root 以外で依存方向を反転できない。

## 対象責務と非対象

session inventory/workspace resolution と setup execution の port を domain/usecase 側の安定境界に置き、composition root で infrastructure adapter を注入する。subprocess primitive/env resolver は #495、session feature変更は非対象。

## 受入条件

- [ ] infrastructure が `usecase::session` を import/call しない。
- [ ] usecase が inventory/setup port を所有し、concrete filesystem/process adapter は外側で実装・注入される。
- [ ] agent start store は必要な identity/snapshot を input で受け、session list を隠れて取得しない。
- [ ] architecture import-boundary check が新しい逆依存を拒否する。

## 必須回帰テスト

fake inventory/setup port で session create/start success/failure/order を固定し、production composition test と禁止 import fixture を追加する。

## docs / 移行影響

v1 architecture/port ownership docs を更新する。永続 format と利用者挙動の migration はない。
