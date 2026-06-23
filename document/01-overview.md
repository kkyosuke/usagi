# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ ← 前へ [0. チュートリアル](00-tutorial.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [これは何か](#これは何か)
- [解決する課題](#解決する課題)
- [主な機能](#主な機能)
- [プロジェクト構造](#プロジェクト構造)
- [技術スタック](#技術スタック)
- [動作に必要なツール](#動作に必要なツール)
- [usagi.ai との関係](#usagiai-との関係)

## これは何か

`usagi` は、AI Agent CLI（Claude Code など）を効率的に使うための開発支援 CLI / TUI アプリケーションです。

リポジトリの初期化（clone + 設定生成）から、worktree ベースのワークスペース管理、AI への指示、
対話ターミナルの起動までを 1 つの TUI に統合します。

## 解決する課題

AI エージェントと並行して開発を進める際に発生する以下の課題を解決します。

- 複数ブランチ・複数タスクを同時に進めるための **Git worktree の管理が煩雑**
- リポジトリごとの初期化・設定がバラバラで、**AI エージェントを動かすまでの準備に時間がかかる**
- どのワークスペースで何をしたかという **作業履歴が散逸する**

worktree を作業単位（セッション）としてまとめ、複数リポジトリ構成でも一括で扱えるようにする仕組みが
`usagi` の中核です。詳しくは [4. オーケストレーション](04-orchestration.md) を参照してください。

## 主な機能

- **CLI コマンド**: `usagi init` / `hop` / `run`（うさぎアニメのギャラリー）/ `status` / `config` / `doctor` / `issue`（タスク管理）/ `mcp`（issue・memory・session の MCP サーバ）/ `llm-mcp`（ローカル LLM MCP サーバ）。
- **TUI（`usagi hop`）**: 起動スプラッシュに続く、起動画面・プロジェクト選択・新規作成・設定・ホームの 5 画面。
- **TUI 内コマンド**: ホーム画面のコマンドモードで実行する `session` / `terminal` / `agent` /
  `history` / `man` など。

コマンドの一覧・引数は [3. コマンドリファレンス](03-commands/README.md)、画面ごとの詳細は
[design/](design/README.md) を参照してください。

## プロジェクト構造

`usagi init` 済みプロジェクトのレイアウト（永続化ファイルの詳細は [data/](data/README.md) が正本）。

```text
<project-root>/             # git リポジトリでなくてもよい（複数リポジトリのルートでも可）
├── .usagi/
│   ├── .gitignore      # .usagi/ 配下の git 管理を制御（usagi が生成）
│   ├── state.json      # 初期化フラグ・セッション一覧
│   ├── settings.json   # ローカル設定（グローバル設定の上書き）
│   ├── history.jsonl    # コマンド実行履歴
│   ├── issues/         # タスク（frontmatter 付き markdown + index.json）
│   ├── memory/         # エージェントのメモリ（frontmatter 付き markdown + MEMORY.md + index.json）
│   └── sessions/       # セッションごとの worktree / コピーを集約（.gitignore 済み）
│       └── <name>/     # session create <name> で作られる作業ツリー（ルート構造を再現）
├── main/               # クローンされたリポジトリ（名前は常に main/）
└── usagi.config        # リポジトリ URL などの設定ファイル
```

加えて OS 標準のデータディレクトリ（既定 `~/.usagi/`）に `workspaces.json`（グローバルレジストリ）と
`settings.json`（アプリ設定）を持ち、初期化済みプロジェクトと設定をシステム全体で追跡します。

## 技術スタック

Rust (edition 2021) で実装。clap（CLI）/ console + crossterm（TUI）/ portable-pty + vt100（埋め込みターミナル）/
システムの git コマンド / serde（永続化）が中核です。**使用クレートと用途の一覧は
[2. アーキテクチャ#技術スタック](02-architecture.md#技術スタック) を参照してください。**

## 動作に必要なツール

| ツール | 要否 | 用途 |
|---|---|---|
| Git | 必須 | クローン・worktree 管理・状態検査 |
| Bash（macOS/Linux）/ cmd.exe（Windows） | 必須 | 埋め込みターミナルで起動するシェル |
| Agent CLI（`claude` / `codex` / `gemini` など） | 任意 | `agent` コマンドで起動する AI エージェント本体 |
| Ollama | 任意 | ローカル LLM 委譲を有効化したときの実行環境 |

導入状況は `usagi doctor` で確認できます（git / bash の有無、デスクトップ通知の可否、設定ストレージの健全性、
ローカル LLM 有効時は ollama とモデルの有無）。`usagi doctor --fix` は OS のパッケージマネージャ
（brew / apt / dnf / pacman）経由での導入を試行し、自動修復できないものは手動手順を表示します。詳細は
[3. コマンドリファレンス](03-commands/01-cli.md#usagi-doctor) を参照してください。

## usagi.ai との関係

`usagi` は先行プロジェクト [usagi.ai](https://github.com/KKyosuke/usagi.ai) をベースに、その設計・コマンド体系を
引き継いで Rust で再構築したものです。usagi.ai は本プロジェクトの起源であり、現在の仕様は本リポジトリの
`document/` 配下が正本です。
