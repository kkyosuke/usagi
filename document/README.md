# usagi ドキュメント

`usagi` の仕様・設計ドキュメントの目次です。開発者・AI エージェントの双方が読む前提でまとめています。
番号順に読むとプロジェクトの全体像 → 詳細へと辿れます。

## 目次

### 概説

| # | ドキュメント | 内容 |
|---|---|---|
| 0 | [00-tutorial.md](00-tutorial.md) | はじめての usagi（インストール 〜 Agent でセッションを並行起動するまでの導入ガイド） |
| 1 | [01-overview.md](01-overview.md) | プロジェクト概要・解決する課題・全体構造 |
| 2 | [02-architecture.md](02-architecture.md) | クリーンアーキテクチャ 4 層・依存ルール・`src/` のモジュール構成 |
| 3 | [03-commands/](03-commands/README.md) | CLI コマンド・TUI 内コマンド・MCP サーバのリファレンス |
| 4 | [04-orchestration.md](04-orchestration.md) | セッション・worktree オーケストレーションの概念とライフサイクル |
| 5 | [05-settings.md](05-settings.md) | 設定項目・保存場所・変更方法・環境変数 |
| 6 | [06-conventions.md](06-conventions.md) | 開発規約（ブランチ・コミット・PR・ドキュメント規約・品質チェック・hooks） |

### 画面設計 — [design/](design/README.md)

| # | ドキュメント | 画面 |
|---|---|---|
| — | [design/README.md](design/README.md) | 画面一覧・画面遷移図・共通設計メモ |
| 1 | [design/01-welcome.md](design/01-welcome.md) | 起動画面（Welcome） |
| 2 | [design/02-open.md](design/02-open.md) | プロジェクト選択画面（Open） |
| 3 | [design/03-new.md](design/03-new.md) | 新規プロジェクト画面（New） |
| 4 | [design/04-config.md](design/04-config.md) | 設定画面（Config） |
| 5 | [design/05-home.md](design/05-home.md) | ホーム画面（Home） |

### データ永続化 — [data/](data/README.md)

| # | ドキュメント | 層 |
|---|---|---|
| — | [data/README.md](data/README.md) | 2 層の概要・共通方針・関連モジュール |
| 1 | [data/01-global.md](data/01-global.md) | usagi 全体（`~/.usagi/` の `workspaces.json` / `settings.json`） |
| 2 | [data/02-workspace.md](data/02-workspace.md) | workspace 毎（`<repo>/.usagi/` の `state.json` / `settings.json` / `history.json`） |
| 3 | [data/03-issues.md](data/03-issues.md) | タスク issue（`<repo>/.usagi/issues/` の markdown + `index.json`） |

## 関連

- [../README.md](../README.md) — リポジトリの README（インストール・使い方）
- [../.agents/workflow.md](../.agents/workflow.md) — AI エージェント向けの開発ワークフロー
