---
number: 26
title: agent コマンド（埋め込みターミナルで Agent CLI を起動）
status: done
priority: medium
labels: [tui]
dependson: [6]
related: []
created_at: 2026-06-16T23:04:26.734525+00:00
updated_at: 2026-06-16T23:08:24.148884+00:00
---

# `agent` コマンド（埋め込みターミナルで Agent CLI を起動）

## 概要

`terminal` を開いてから Agent CLI（`claude` など）を手で入力する、という一連の操作をまとめた TUI 内コマンドです。`agent` を実行すると、`terminal` と同じ右ペイン埋め込みシェルが起動し、設定中の Agent CLI が自動入力されます。実質「`terminal` → `claude`」のショートカットです。

一括起動の AI 対話（プロンプトを渡す `ai <prompt>`、[005](005-ai.md)）とは別物で、こちらは対話型ターミナルにそのまま Agent CLI を立ち上げる点が異なります。

## やること

- `agent` で選択中の worktree（先頭のルート行を選んでいればワークスペースルート。[031](031-root-mode.md) 参照）を作業ディレクトリに、`terminal` と同じ埋め込みシェルを起動する。
- 起動直後にシェルへ Agent CLI の起動コマンドを送る（実質 `terminal` → コマンド入力）。
- 起動する Agent CLI は実効設定（グローバル設定にローカル上書きを適用）の `agent_cli` から解決する（既定は `claude`、`gemini` などに変更可）。
- 起動コマンドには usagi 自身の issue MCP サーバ（`usagi mcp`）を、対応する Agent CLI へ組み込む。Claude はインラインの `--mcp-config` で注入する（Gemini はインライン注入用フラグを持たないため現状は素のまま起動）。
- Agent / シェルの終了、または `Ctrl-O`（デタッチ）でコマンドモードへ戻る。

## 完了条件

- `agent` で `terminal` と同等の埋め込みシェルが開き、設定中の Agent CLI が起動する。
- 設定（Config 画面・ローカル設定）の Agent CLI 選択が起動コマンドに反映される。
- 起動した Agent CLI から usagi の issue MCP tool（`issue_create` / `issue_list` など）が利用できる（対応 CLI のみ）。
- Agent 終了後・`Ctrl-O` でワークスペース画面のコマンドモードへ正しく復帰する。

## 実装状況

`terminal`（[006](006-terminal.md)）の仕組みをそのまま再利用して実装。

- `domain/settings.rs`：`AgentCli::command()` で各 Agent CLI の起動コマンド名（`claude` / `gemini`）を解決。`AgentCli::launch_command()` は起動コマンド**行**を返し、対応 CLI には usagi の issue MCP サーバ（`usagi mcp`）を組み込む（Claude は `--mcp-config '{"mcpServers":{"usagi":…}}'`、Gemini は素のまま）。
- `presentation/tui/home/command.rs`：`agent` コマンドを追加し、`Effect::OpenAgent` を返す。
- `presentation/tui/home/event.rs`：`Effect::OpenAgent` を `OpenTerminal` と同じ経路で処理し、`open_terminal` コールバックへ「Agent として開く」フラグ（`true`）を渡す。
- `presentation/tui/home/terminal_pane.rs`：`run` に `initial` 引数を追加し、`Some(command)` のときシェル起動直後にそのコマンド行を送る（端末 I/O のためカバレッジ計測対象外）。
- `presentation/tui/home/mod.rs`：実効設定から Agent CLI を解決し、`AgentCli::launch_command()` で MCP サーバ込みの起動コマンド行を得る（読み取り失敗時は既定の `claude` にフォールバック）。`agent` フラグが立っているときだけそのコマンド行を `terminal_pane::run` へ渡す（ワイヤリングのためカバレッジ計測対象外）。
