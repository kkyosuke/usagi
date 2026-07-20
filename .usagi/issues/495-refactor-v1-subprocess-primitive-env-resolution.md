---
number: 495
title: refactor(v1): subprocess primitive と env resolution の逆依存を解消する
status: todo
priority: medium
labels: [review, v1, architecture, process]
dependson: [500]
related: [59, 113, 171, 189]
parent: 453
created_at: 2026-07-20T12:07:07.377791+00:00
updated_at: 2026-07-20T12:07:43.112630+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/infrastructure/env_resolver/op_cli.rs` が `crate::presentation::mcp::child_io::{read_capped,wait_with_timeout,WaitableChild}` と `crate::usecase::settings` を import する。infrastructure が presentation/usecase の具体実装を呼ぶ逆依存で、process timeout primitive の再利用と layer testability を壊している。

## 成立条件 / 再現フロー

module import graph を列挙すると infrastructure→presentation/usecase edge が現れる。process helper や settings API の変更が低層 adapter を上層へ compile-time 結合し、別 entrypoint から独立利用できない。

## 対象責務と非対象

#500 で有界化した subprocess primitive を infrastructure-neutral な lower module/crate へ置き、env resolver へ resolved settings/config port を注入する。session inventory/setup の逆依存は #496、env provider feature追加は非対象。

## 受入条件

- [ ] infrastructure module が presentation/usecase module を import しない。
- [ ] process spawn/wait/read primitive は lower-level port/adapter として MCP/env/release 等から共有できる。
- [ ] env resolver は resolved immutable settings または lower-level config port を input に取り、global usecase を直接呼ばない。
- [ ] architecture boundary を自動 import/lint test で固定する。

## 必須回帰テスト

env resolution success/failure/timeout、settings variations を fake ports で検証し、禁止 dependency fixture が architecture check を失敗させることを固定する。

## docs / 移行影響

v1 layer/port diagram を更新する。runtime behavior/data migration はなく、#500 の timeout semantics を維持する。
