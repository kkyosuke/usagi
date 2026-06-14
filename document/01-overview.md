# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [これは何か](#これは何か)
- [解決する課題](#解決する課題)
- [主な機能](#主な機能)
- [プロジェクト構造（想定）](#プロジェクト構造想定)
- [技術スタック](#技術スタック)
- [動作に必要なツール](#動作に必要なツール)
- [usagi.ai との関係](#usagiai-との関係)

## これは何か

`usagi` は、AI Agent CLI（Claude Code など）を効率的に使うための開発支援 CLI / TUI アプリケーションです。
先行プロジェクト [usagi.ai](https://github.com/KKyosuke/usagi.ai) をベースに、その設計・機能を引き継いで再構築します。

`usagi` はリポジトリの初期化（clone + 設定生成）から、worktree ベースのワークスペース管理、AI への指示、
対話ターミナルの起動までを 1 つの TUI に統合します。

## 解決する課題

AI エージェントと並行して開発を進める際に発生する以下の課題を解決します。

- 複数ブランチ・複数タスクを同時に進めるための **Git worktree の管理が煩雑**
- リポジトリごとの初期化・設定がバラバラで、**AI エージェントを動かすまでの準備に時間がかかる**
- どのワークスペースで何をしたかという **作業履歴が散逸する**

worktree を作業単位（セッション）としてまとめ、複数リポジトリ構成でも一括で扱えるようにする仕組みが
`usagi` の中核です。詳しくは [4. オーケストレーション](04-orchestration.md) を参照してください。

## 主な機能

- **CLI コマンド**: `usagi init` / `hop` / `status` / `doctor` / `issue`（タスク管理）など。
- **TUI（`usagi hop`）**: 起動画面・プロジェクト選択・新規作成・設定・ホームの 5 画面。
- **TUI 内コマンド**: ホーム画面のコマンドモードで実行する `session` / `terminal` / `ai` /
  `history` / `man` など。

コマンドの一覧・引数・実装状況は [3. コマンドリファレンス](03-commands/README.md)、画面ごとの詳細は
[design/](design/README.md) を参照してください。

## プロジェクト構造（想定）

```text
<project-root>/             # git リポジトリでなくてもよい（複数リポジトリのルートでも可）
├── .usagi/
│   ├── state.json      # 初期化フラグ・セッション / worktree 一覧
│   ├── history.json    # コマンド実行履歴
│   └── worktree/       # セッションごとの worktree / コピーを集約（.gitignore 済み）
│       └── <name>/     # session new <name> で作られる作業ツリー（ルート構造を再現）
├── main/               # クローンされたリポジトリ（名前は常に main/）
├── usagi.config        # リポジトリ URL などの設定ファイル
└── .gitignore          # .usagi/ を無視する設定が追記される
```

加えて、OS 標準のデータディレクトリ（既定 `~/.usagi/`）に `workspaces.json`（グローバルレジストリ）と
`settings.json`（アプリ設定）を持ち、初期化済みプロジェクトと設定をシステム全体で追跡します。
永続化の詳細は [data/](data/README.md) を参照してください。

## 技術スタック

usagi.ai を踏襲し、Rust で実装します。

- **言語**: Rust (edition 2021)
- **CLI**: clap
- **TUI**: ratatui + crossterm
- **疑似ターミナル**: portable-pty + vt100
- **Git 操作**: git2 / システムの `git` コマンド（読み取り専用検査）
- **AI 連携**: llm / llama-cpp-2
- **非同期処理**: tokio
- **シリアライズ**: serde / serde_json

層構成・依存方向・モジュールの配置は [2. アーキテクチャ](02-architecture.md) を参照してください。

## 動作に必要なツール

- **Git**（必須）: クローン・worktree 管理
- **Bash**（macOS/Linux）または **cmd.exe**（Windows）（必須）: 対話ターミナル
- **AWS CLI**（必須）: AWS SSO ログインなど
- **Node.js / npm、Python**（任意）: AI エージェントや開発環境で利用

導入状況は `usagi doctor` で確認できます（[3. コマンドリファレンス](03-commands/README.md) 参照）。不足ツールは
`usagi doctor --fix` で OS のパッケージマネージャ経由の導入を試行でき、修復できないものは手動手順が表示されます。

## usagi.ai との関係

| | usagi.ai | usagi（本プロジェクト） |
|---|---|---|
| 位置づけ | 先行実装 | usagi.ai をベースとした後継プロジェクト |
| リポジトリ | `KKyosuke/usagi.ai` | `KKyosuke/usagi` |
| 設計・機能 | — | アーキテクチャ・コマンド体系を継承 |

詳細な仕様は usagi.ai の `doc/` 配下（CLI リファレンス、TUI 仕様、アーキテクチャ）を参照しつつ、
本プロジェクトの `document/` 配下に順次移植・更新していきます。移植対象の機能一覧は
[../issues/README.md](../issues/README.md) にまとまっています。
