---
number: 609
title: fix(daemon): sakana-ai の codex-fugu readiness を認識する
status: todo
priority: medium
labels: [review, v2, daemon, agent, sakana-ai, correctness]
dependson: []
related: [545, 582]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-07-31T15:00:00+09:00
---

## Finding（P2 correctness）

`DefaultModel::SakanaAi::command` と `CodexAdapter::sakana` は `codex-fugu` を launch program にする一方、`src/runtime/daemon.rs::SystemAgentReadiness::ready` は product が `codex` / `claude` の場合しか認めず、それ以外を即 `Err(())` にする。`RootCodexProvisioner` は program を readiness product として渡すため、install 済み `codex-fugu` も常に unavailable になる。#582 は MCP schema/allowlist の別境界であり本欠陥を直さない。

## 最小修正方針

profile→executable→readiness command を shared catalog にし、`codex-fugu` に対応する非 secret status probe を定義する。未知 executable は従来どおり fail closed にする。

## テストと受け入れ条件

- fake `codex-fugu` の成功/失敗 status が sakana-ai admission の成功/安全な拒否へ反映される。
- `codex` / `claude` の argv と secret 非露出契約は不変。
- schema に表示された install 済み sakana-ai を実際に launch できる E2E がある。
