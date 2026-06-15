---
number: 032
feature: local-llm-mcp
title: ローカル LLM を MCP として公開しクラウド Agent のトークン消費を抑える
status: done
priority: medium
category: mcp
dependson: [019, 025]
ref: PR (this branch)
---

# ローカル LLM の MCP 公開（`usagi llm-mcp`）

## 概要

ローカルで動く LLM（Ollama 経由）を MCP（Model Context Protocol）サーバとして公開し、
クラウド Agent（Claude Code 等）が要約・命名・定型文生成・単純変換などの軽量タスクを
ローカル LLM に委譲できるようにします。クラウド側の（課金対象の）トークン消費を抑えることが目的です。

usagi が勝手に有効化することはありません。次の 3 点を満たします。

1. **config で on にする** — 設定（グローバル `settings.json` の `local_llm`）で明示的に有効化する。
   未インストール時は Config 画面で「Install」と表示し、インストール後は on/off トグルに変わる。
2. **資材がなければ install する** — `ollama` 本体とモデルが無ければ導入する。
   Config 画面の Install アクション、`usagi doctor --fix`、いずれからも実行できる。モデル選択時にも導入する。
3. **MCP として Agent に渡す** — 有効時、Agent 起動コマンド（`--mcp-config`）に
   `usagi-llm` サーバを追加し、`local_llm_ask` ツールを公開する。

## 公開するツール

| ツール | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `local_llm_ask` | `prompt` | `system` | ローカルモデルの補完テキスト |

## やること

- `domain/settings.rs`: `LocalLlm { enabled, model }` を `Settings` に追加。`LocalSettings` に
  `local_llm_enabled` 上書きを追加。`AgentCli::launch_command` / `Settings::agent_launch_command`
  で有効時に `usagi-llm` MCP サーバと委譲を促すシステムプロンプトを wire する。
- `presentation/mcp_llm.rs`: `LlmMcpServer`（JSON-RPC ディスパッチ）と `LlmBackend` トレイト。
  本番の Ollama バックエンド（`ollama run` へシェルアウト）は `presentation/cli/llm_mcp.rs`。
- `usecase/local_llm.rs`: `ollama` / モデルの有無判定とインストール（`doctor::CommandRunner` を再利用）。
- `usecase/doctor.rs` / `presentation/cli/doctor.rs`: 有効時にローカル LLM の健全性チェックと
  `--fix` での導入を統合。
- `presentation/tui/config/`: Config 画面に Local LLM（Install / on-off）と Local LLM Model の行を追加。

## 完了条件

- config で有効化でき、未導入時は Config から、または `usagi doctor --fix` から導入できる。
- 有効時に Agent 起動コマンドへ `usagi-llm` サーバが追加され、`local_llm_ask` が呼べる。
- 既定は off（usagi が勝手に有効化しない）。
- カバレッジ 100% を維持する。

> 依存: [019-doctor-fix](019-doctor-fix.md)（依存自動修復の枠組み）と
> [025-issue-mcp](025-issue-mcp.md)（MCP サーバ実装パターン）を前提とします。
> 関連: [005-ai](005-ai.md) の Agent 起動と組み合わせて使います。
