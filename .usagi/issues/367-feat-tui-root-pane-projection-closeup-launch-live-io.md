---
number: 367
title: feat(tui): root pane projection と closeup/launch・live IO
status: done
priority: high
labels: [tui, workspace-root, agent, terminal]
dependson: [364, 365, 366]
related: []
parent: 363
created_at: 2026-07-19T21:05:29.225971+00:00
updated_at: 2026-07-19T22:12:25.381927+00:00
---

## 目的

`Target::Root` active で Agent/Terminal を作成し、root pane に投影して双方向 IO（出力・入力・resize・detach/reconnect）を成立させる。session pane の投影は回帰させない。

## 変更内容

- `crates/tui/src/usecase/application/agent_runtime.rs`
  - pane host の key を `SessionId` から `Target` へ。`sync_live_pane` の `Target::Root(_) => false` を撤去し root pane の live を判定。`dispatch`/`pane`/`input`/`resize`/`reconnect`/`stream`/`select_live` を Target 対応に。
- `crates/tui/src/usecase/application/controller.rs`
  - `submit_closeup` の root agent 拒否（"workspace root cannot start an agent"）を撤去。`Effect::LaunchAgent` を root 対応（`Target` を運ぶ／`session: Option`）。
- `crates/tui/src/presentation/workspace_runtime.rs`
  - `on_effect` の `OpenTerminal`/`LaunchAgent` を `Target::Root` でも pane 要求するよう拡張。
- root scope の構築: 本番 `TerminalScopePort` 相当が `Target::Root` を `session_id: None` + daemon 公開の root worktree id へ解決。root worktree id / path を snapshot から controller/projection に配線。`AgentLaunchIntent` を root 対応。

## 完了条件

- `⌂ root` active で terminal/agent の pending→live 昇格と双方向 IO が動作する。
- session pane（keyed by session）の投影・live 判定・IO の回帰テストが green。
- coverage 100%。

## 依存

#364（core 語彙）、#365 / #366（daemon 経路）。
