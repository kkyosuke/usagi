---
number: 656
title: fix(daemon): Agent readiness probe を owner lock 外の bounded preflight にする
status: in-progress
priority: high
labels: [review, v2, daemon, agent, freeze, process, resilience]
dependson: []
related: []
parent: 654
created_at: 2026-08-05T13:40:18.097359+00:00
updated_at: 2026-08-05T22:18:25.532258+00:00
---

## Finding（P1 freeze / daemon availability）

Agent launch / exact resume の IPC handler は `SharedAgentRuntime` の mutex を取得したまま `AgentRuntime::launch` / `resume_exact` を実行する。launch 内の adapter resolve は composition の `SystemAgentReadiness::ready` に到達し、`claude auth status` / `codex login status` / `codex-fugu login status` を `Command::status()` で timeout 無しに同期実行する。

この subprocess が credential prompt、CLI bug、壊れた home/network 等で hang すると、Agent owner mutex が無期限に保持される。同じ mutex を使う inventory、terminal attach/input、phase/caller validation、observer output/exit commit が止まり、TUI は client deadline 後に戻れても daemon-side owner と subprocess は残り続ける。pane launch worker へ移しただけでは daemon freeze を防げない。

## 修正方針

- readiness command の executable / argv は `DefaultModel::readiness_command` を SSoT のまま使う。
- subprocess lifecycle を owner-lock 外の bounded preflight に分離する。timeout 時は exact child を terminate → bounded grace → kill → reap し、stdout/stderr/credential/path を response や log に載せない。
- preflight 後に owner lock を取り、generation/scope/config/executable/concurrency/idempotency を再検証してから reservation/spawn する。古い preflight 結果だけで effect を admit しない。
- 同一 provider の同時 probe は bounded/coalesced にし、client 数だけ subprocess/thread を無制限生成しない。

## 受入条件

- hung readiness fixture 中も Agent inventory、existing terminal input/snapshot、phase report、shutdown が bounded latency で進む。
- timeout/cancel 後に child・pipe・reader thread・zombie が残らない。
- concurrent launch burst の probe concurrency/queue が hard bound を超えない。
- probe 後の config/runtime/generation 変更は spawn 前再検証で拒否される。
- authenticated / unauthenticated / executable missing の既存 safe error mapping を維持する。

## 根拠箇所

- `src/runtime/daemon.rs`: `dispatch_agent`、`SystemAgentReadiness::ready`
- `crates/daemon/src/usecase/runtime.rs`: owner 内の `adapter.resolve`
- `crates/core/src/domain/settings/mod.rs`: readiness command vocabulary
