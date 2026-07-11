# usagi ドキュメント

`usagi` の仕様・設計ドキュメントの目次です。開発者・AI エージェントの双方が読む前提でまとめています。
番号順に読むとプロジェクトの全体像 → 詳細へと辿れます。

> **リポジトリレイアウト**: リポジトリルートは v2（フルリライト）の Cargo パッケージで、
> 旧実装（v1）は本ディレクトリを含めて `v1/` に退避してある（[v1 の README](../README.md)）。
> 機能・画面・データ仕様は v1 実装時点の記述である。開発規約（[06-conventions.md](06-conventions.md)）は
> v1/v2 共通で引き続き有効である。

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
| 7 | [07-test-observability.md](07-test-observability.md) | slow/flaky test の計測方法・local sccache opt-in・nextest 採否・基準値 |

### 画面設計 — [design/](design/README.md)

| # | ドキュメント | 画面 |
|---|---|---|
| — | [design/README.md](design/README.md) | 画面一覧・画面遷移図・共通設計メモ |
| 0 | [design/00-splash.md](design/00-splash.md) | 起動スプラッシュ（Splash） |
| 1 | [design/01-welcome.md](design/01-welcome.md) | 起動画面（Welcome） |
| 2 | [design/02-open.md](design/02-open.md) | プロジェクト選択画面（Open） |
| 3 | [design/03-new.md](design/03-new.md) | 新規プロジェクト画面（New） |
| 4 | [design/04-config.md](design/04-config.md) | 設定画面（Config） |
| 5 | [design/home/README.md](design/home/README.md) | ホーム画面（Home） |

### 設計提案 — [proposals/](proposals/README.md)

未実装の運用モデル・機構の設計判断を記録する（正本 spec とは分離。詳細は [proposals/README.md](proposals/README.md)）。

| # | ドキュメント | 内容 |
|---|---|---|
| — | [proposals/README.md](proposals/README.md) | 設計提案の位置づけと一覧 |
| 1 | [proposals/01-root-orchestration.md](proposals/01-root-orchestration.md) | 自律オーケストレーション運用モデル（正本へ畳み込み済み → [04-orchestration.md](04-orchestration.md#自律オーケストレーション運用モデル)） |
| 4 | [proposals/04-sccache-rust-builds.md](proposals/04-sccache-rust-builds.md) | sccache による Rust ビルド高速化の導入計画 |
| 5 | [proposals/05-session-lifecycle.md](proposals/05-session-lifecycle.md) | session lifecycle の `state.json` 一元化、競合制御、クラッシュ回復 |

### データ永続化 — [data/](data/README.md)

| # | ドキュメント | 層 |
|---|---|---|
| — | [data/README.md](data/README.md) | 2 層の概要・共通方針・関連モジュール |
| 1 | [data/01-global.md](data/01-global.md) | usagi 全体（`~/.usagi/` の `workspaces.json` / `settings.json`） |
| 2 | [data/02-workspace.md](data/02-workspace.md) | workspace 毎（`<repo>/.usagi/` の `state.json` / `settings.json` / `history.json`） |
| 3 | [data/03-issues.md](data/03-issues.md) | タスク issue（`<repo>/.usagi/issues/` の markdown + `index.json`） |
| 4 | [data/04-memory.md](data/04-memory.md) | エージェントのメモリ（`<repo>/.usagi/memory/` の markdown + `MEMORY.md` + `index.json`） |
| 5 | [data/05-orchestrators.md](data/05-orchestrators.md) | durable orchestrator の plan・claim・event 保存形式 |

## 関連

- [../README.md](../README.md) — リポジトリの README（インストール・使い方）
- [../../.agents/workflow.md](../../.agents/workflow.md) — AI エージェント向けの開発ワークフロー
