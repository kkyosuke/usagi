---
number: 757
title: refactor(root): 合成ルート daemon.rs / dispatch.rs から decode・authorization・response shaping を daemon 面へ移す
status: todo
priority: high
labels: [v2, refactor, root, daemon, coverage]
dependson: []
related: [758, 763]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`document/02-architecture.md` は合成ルートを「面の選択だけを担う」と定義するが、`src/` は 58,237 行あり、
`src/runtime/daemon.rs` 単体で 25,730 行（production 約 11,300 行、top-level 型 95、`impl` 117、`Drop` impl 14、
`Arc<Mutex<_>>` 18）ある。直近 400 commit の変更回数は 152 回で workspace 最多である。

`#[coverage(off)]` は workspace 全体 535 件のうち 405 件（76%）が `src/` に集中し、除外された関数本文の合計は約 12,000 行
に達する。とくに `src/runtime/daemon/dispatch.rs` と `dispatch/session.rs` は `reason=composition` 45 件で次の関数を除外している。

| 関数 | 行数 |
|---|---|
| `dispatch_agent_tool` | 817 |
| `dispatch_session_action` | 586 |
| `start_ipc_accept_loop`（daemon.rs） | 454 |
| `dispatch_user_decision` | 327 |
| `spawn_ipc_server`（daemon.rs） | 314 |
| `dispatch_supervisor_tool` | 295 |
| `delegate_brief` | 279 |
| `dispatch_rollover` | 207 |
| `dispatch_agent` | 202 |

これらは `serde_json::Value`（dispatch.rs で 53 参照）の decode、caller authorization、response envelope の shaping を含み、
`document/06-conventions.md` の `coverage(off)` 例外が許可理由にしない「parser、validation、error mapping」に当たる。
business regression がここに入っても coverage gate は検知しない。

## 方針

- request family ごとの decode / validate / response shaping を typed handler として `crates/daemon/src/presentation/ipc`
  （transport-independent な server loop 側）へ移し、合成ルートの `dispatch*` は「注入済み owner / store を handler に渡す」
  wiring だけにする。移した handler は fake port の unit test で直接固定する。
- `src/runtime/daemon.rs` は、既に architecture test で分離済みの `tenant_control` / `dispatch` / `agent_provisioning` と同じ
  やり方で、責務ごとの子 module（`ipc_accept` / `standby` / `bootstrap_broker` / `instance_lock` / `pty` / `background_workers`）
  へ分割し、`tests/architecture.rs` に「戻さない」guard を追加する。
- `coverage-off-budget.json` の `root-cli` / `daemon` 件数を減らす方向でのみ更新し、`composition` 例外は
  「実 IO と注入済み依存を束ねるだけ」の関数に限る。

## 完了条件

- `src/runtime/daemon/dispatch*.rs` に 100 行を超える `#[coverage(off)]` 関数が残らない。
- `src/runtime/daemon.rs` が 3,000 行以下になり、分割先 module を `document/02-architecture.md` の責務表へ追記する。
- `coverage-off-budget.json` の `src/` 合計が着手前より減る。
- `cargo test --workspace` と coverage 100% が CI で green。
