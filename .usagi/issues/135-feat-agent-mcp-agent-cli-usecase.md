---
number: 135
title: feat(agent): インストール済み かつ MCP 対応の agent CLI を列挙する usecase と、セッション委譲時の検証
status: done
priority: high
labels: [orchestration, agent]
dependson: []
related: [99]
created_at: 2026-07-05T00:29:37.866083+00:00
updated_at: 2026-07-05T00:29:37.866083+00:00
---

## 背景

session に agent を指定できるようにする（#099）際、指定を受け付ける前に「利用可能な agent か」を検証・フィルタしたい。「利用可能」は 2 観点:

- (a) **インストール済み**: `usecase/agent.rs::available_clis(runner)`（`AgentCli::ALL` を `runner.available(cmd)` で絞る。実体は `<cmd> --version`）。
- (b) **MCP 注入可能**: `domain/agent_feature.rs::support(cli, AgentFeature::Mcp) == Yes`（SSoT。exhaustive match）。

現状 (b) は Claude / Codex / codex-fugu のみ Yes、gemini / agy は No。gemini/agy に MCP を足す issue（related）が land すると行列が Yes に反転し、**本 issue のロジックを変えずに gemini/agy が自動で候補に入る**（「対応前は候補外／対応後に解禁」が行列 1 か所で成立）。

## やること

- `usecase/agent.rs` に `mcp_capable_clis(runner: &dyn CommandRunner) -> Vec<AgentCli>` を追加（`available_clis` を `agent_feature::support(cli, Mcp)==Yes` で更に絞る）。`available_clis` は温存（インストール済みだけ見たい TUI ピッカー用途は残る）。
- `session_create` / `session_delegate_issue`（MCP、#099 が `agent_cli` 引数を追加）で受けた `agent_cli` を検証:
  - インストール済みでない → ツールエラー（`mcp_capable_clis` を候補列挙するメッセージ）。
  - MCP 非対応 → **原則ツールエラー**（委譲作業は usagi MCP を前提とするため）。※「警告して起動は許す」案もあり、厳格さは要相談。
  - `agent_cli` 省略時は検証せず従来フォールバック（実効 `settings.agent_cli`）。
- **agy `--version` レイテンシ/不在対策**: `CommandRunner::available`（`<cmd> --version`）が agy で無い/遅い懸念に対し、probe のタイムアウト導入、または PATH 存在（`which`）ベースの軽量判定への切替を検討・実装。TUI ピッカーは probe を別スレッドで後追い適用済みなので UI ブロックは既に回避されている点も踏まえる。

## 受け入れ条件

- `mcp_capable_clis` が「インストール済み ∧ `agent_feature` で Mcp=Yes」の CLI のみを `AgentCli::ALL` 順で返す。
- gemini/agy MCP 注入 issue が land する前は claude/codex(+fugu) のみ、land 後は gemini/agy も候補に入る（本 issue のコード変更なしで）。
- 委譲時に不正な `agent_cli` が明確なエラー（候補列挙付き）になる。
- agy の probe がハング/長時間ブロックしない。
- 既存テスト緑、カバレッジ 100% 維持（`FakeRunner` でユニットテスト）。
