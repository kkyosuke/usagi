---
number: 760
title: refactor(daemon): supervisor_runtime.rs / agent_ipc.rs / session_runtime.rs を bounded context 別に分割し tests を隣接ファイルへ出す
status: todo
priority: high
labels: [v2, refactor, daemon]
dependson: []
related: [757, 762]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`crates/daemon/src/usecase/` の 3 ファイルが 1 万行規模で、production とインライン `mod tests` が同居している。

| ファイル | 総行数 | production | 型 |
|---|---|---|---|
| `supervisor_runtime.rs` | 14,399 | 約 4,800 | 29 |
| `agent_ipc.rs` | 13,207 | 約 4,900 | 21 |
| `session_runtime.rs` | 7,482 | 約 2,800 | — |

`supervisor_runtime.rs` には DecisionWake、ArtifactVerification、Goal / Caller / Delegated の Promotion、Dispatch Reservation、
KeyTombstones、RuntimeState が、`agent_ipc.rs` には scope 解決、admission、prompt / report delivery、MCP caller lease、
terminal actor、runtime store port がそれぞれ 1 ファイルに入っている。`admit_resume_exact`（237 行）、`admit`（205 行）、
`start_scoped`（292 行）、`reserve_delegated_dispatch_inner`（174 行）など 150 行超の関数が集まり、
`clippy::cognitive_complexity` 25 超の production 関数 18 件のうち 10 件が daemon crate にある。

同じ crate の `usecase/resources/durable.rs` + `durable/tests.rs`、`authority/rollover/tests.rs` は既に「production と
tests を隣接ファイルに分ける」形になっており、3 ファイルだけが古い形で残っている。

## 方針

- `supervisor_runtime/{wake,verification,promotion,reservation,state}.rs`、`agent_ipc/{scope,admission,delivery,mcp_lease,terminal_actor,ports}.rs`
  のように bounded context ごとの子 module へ移し、親 `mod.rs` は re-export と共有型だけにする。
- インライン `mod tests` を `<module>/tests.rs` へ移し、`durable` と同じ配置に揃える。
- 150 行を超える admission 系関数は「検証 → 予約 → 反映」の段で分け、各段を単体で test する。

## 完了条件

- `crates/daemon/src/usecase/` に 3,000 行を超える単一ファイルが無い。
- 該当 module の `cognitive_complexity` 25 超の production 関数が半減する。
- 分割先を `document/05-daemon.md` の module 対応表へ反映する。
