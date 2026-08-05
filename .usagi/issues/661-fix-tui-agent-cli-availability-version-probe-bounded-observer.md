---
number: 661
title: fix(tui): Agent CLI availability と version probe を bounded な共通 observer にする
status: in-progress
priority: medium
labels: [review, v2, tui, core, agent, process, freeze, ssot]
dependson: []
related: [545, 609, 656]
parent: 654
created_at: 2026-08-05T13:49:12.484517+00:00
updated_at: 2026-08-05T23:05:33.376552+00:00
---

## Finding（P2 startup freeze / SSoT drift）

Agent provider語彙は `DefaultModel` に集約されている一方、availabilityの実IO policyは分散している。

- TUI `available_agent_models` は各commandへ `--version` を同期 `output()` し、exit statusを見ずspawn成功だけでinstalled扱いする。Welcome/Config/direct workspace起動前とworkspace離脱後に繰り返し実行され、1 CLIがhangするとraw mode/最初のframeより前で無期限停止する。
- Doctorも同じ `--version` をtimeout無しで実行する。
- daemon dispatchはcore `PathExecutableLocator` のPATH存在判定、Agent launchは別のauth/readiness commandを使う。

「installed」「version取得」「authenticated/ready」は別の事実だが、現在はTUIの `--version` 1回がinstalled判定を兼ね、共通のprocess lifecycle boundがない。語彙はSSoTでも観測責務がdriftしている。

## 修正方針

- provider mappingは引き続き `DefaultModel` をSSoTとする。
- picker/Configのinstalled集合は共通 `ExecutableLocator`（PATH lookup、実行なし）からprocess lifetimeに1 snapshot作り、全entry/Closeup/Directorへ同じ値を注入する。必要なら明示refreshだけで更新する。
- Doctorのversion取得とdaemon readinessは別typed probeとし、共通のbounded child runner（timeout → terminate → kill → reap、bounded stdout/stderr）を利用する。
- exit nonzero、timeout、invalid UTF-8、巨大outputをsafe resultへ正規化し、raw path/env/credentialを表示しない。

## 受入条件

- hung fake CLIがあってもTUI初期frameとDoctorが規定deadline内に返る。
- startup / workspace leave / Config reopenで同じproviderを繰り返しspawnしない。
- Config、Closeup、Director pickerが同じ `AvailableModels` snapshotを読む。
- executable存在、version failure、unauthenticated readinessを別状態としてtestし、daemon request前UI拒否とdaemon最終再検証の責務を混同しない。
- child/pipe/zombieが残らない。

## 根拠箇所

- `src/runtime/tui.rs`: `available_agent_models`, `cli_is_available`, `RuntimeDoctorPort::tool_version`
- `crates/core/src/domain/settings/mod.rs`: `DefaultModel`, `AvailableModels`
- `crates/core/src/infrastructure/runtime_model.rs`: `PathExecutableLocator`
- `src/runtime/daemon.rs`: `SystemAgentReadiness`
