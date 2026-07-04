---
number: 99
title: feat(agent): セッション単位の agent CLI・モデル指定 — モデル注入点（#577）に供給源を接続する
status: todo
priority: high
labels: [orchestration, agent]
dependson: []
related: []
created_at: 2026-07-04T05:09:26.842394+00:00
updated_at: 2026-07-04T05:14:01.144881+00:00
---

## 背景

PR #577 でエージェント CLI（claude / codex / codex-fugu / gemini）の起動コマンドに**モデルを注入できる口**が通ったが、既定 None のまま供給源が未実装。また `agent_cli` はワークスペース単位（グローバル ⊕ ローカル上書き）でしか選べない。コーディネータが「軽いタスクは小さいモデル、重い設計は大きいモデル」とタスクごとに振り分けるには、**セッション単位**で CLI とモデルを指定できる必要がある。

## やること

- `session_create` / `session_delegate_issue`（MCP）に任意引数 `agent_cli` / `model` を追加する。
- 指定を `state.json` の SessionRecord に記録し、そのセッションでの `agent` 起動（自動 spawn・ペイン復旧を含む）時に実効設定より優先して解決する。
- #577 の注入点（`domain/agent` の model 配線）へ接続し、CLI ごとのモデル指定フラグ（claude `--model` 等）に展開する。
- モデル名のバリデーション方針を決める（CLI ごとの allowlist にするか、素通しにするか）。
- TUI からの指定（session create 時のオプション）は必須ではないが、あれば一貫する。

## 受け入れ条件

- `session_delegate_issue(number, agent_cli: "claude", model: "...")` で委譲したセッションが、指定 CLI・指定モデルで起動する。
- 未指定なら従来どおりワークスペースの実効設定（`agent_cli`）と CLI 既定モデルにフォールバックする。
- ドキュメント（03-mcp / 04-orchestration / 05-settings / data/02-workspace）を更新する。
