# usagi — プロジェクト概要

## これは何か

`usagi` は、AI Agent CLI（Claude Code など）を効率的に使うための開発支援 CLI / TUI アプリケーションです。
先行プロジェクト [usagi.ai](https://github.com/KKyosuke/usagi.ai) をベースに、その設計・機能を引き継いで再構築します。

AI エージェントと並行して開発を進める際に発生する以下の課題を解決します。

- 複数ブランチ・複数タスクを同時に進めるための **Git worktree の管理が煩雑**
- リポジトリごとの初期化・設定がバラバラで、**AI エージェントを動かすまでの準備に時間がかかる**
- どのワークスペースで何をしたかという **作業履歴が散逸する**

`usagi` はリポジトリの初期化（clone + 設定生成）から、worktree ベースのワークスペース管理、AI への指示、対話ターミナルの起動までを 1 つの TUI に統合します。

## 主な機能（usagi.ai から継承）

### CLI コマンド

| コマンド | 説明 |
|---|---|
| `usagi init` | カレントディレクトリをプロジェクトとして登録する（`.usagi/` を初期化し、グローバルレジストリに追加） |
| `usagi init --git <URL>` | カレントディレクトリ配下に `<リポジトリ名>/` を作成して clone し、プロジェクトとして登録する |
| `usagi hop` | メインの TUI を起動する。プロジェクト選択 → ワークスペース操作画面へ遷移 |
| `usagi status` | カレントリポジトリの worktree 状態を `.usagi/state.json` に同期し表示する |
| `usagi doctor` | Git / Bash / AWS CLI / Node.js / Python などの依存ツールの導入状況を確認する |

### TUI 内コマンド（`usagi hop` 起動後）

| コマンド | 説明 |
|---|---|
| `session new <name>` / `session list` | セッション（`.usagi/worktree/<name>/` 配下に再帰的に worktree を構築）の作成・一覧 |
| `space` | ワークスペース（worktree）の切り替え |
| `ai` | AI エージェントへの指示・対話 |
| `terminal` | アクティブ worktree を作業ディレクトリとした対話型シェルの起動（TUI を一時退避し、シェル終了後に復帰） |
| `history` | コマンド実行履歴の表示 |
| `doctor` | 依存関係チェック（TUI 版） |
| `man` | コマンドのヘルプ表示 |

> `session new <name>` で作成した worktree はサイドバーの worktree 一覧に表示され、選択した状態で `terminal` を実行するとその worktree で対話シェルが開きます（未選択時はワークスペースルート）。AI エージェント（Claude など）はこのシェルから起動して開発を進められます。

### TUI の画面構成

`usagi hop` は USAGI AI メニュー（Open / New / Config / Quit）から始まり、プロジェクト選択を経てワークスペース画面に遷移します。
ワークスペース画面は「左ペイン：worktree 一覧」「右ペイン：コマンド履歴」「下部：コマンド入力欄」で構成され、サイドメニューモードとコマンドモード（Tab 補完・履歴遡り付き）を切り替えて操作します。

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

加えて、OS 標準のデータディレクトリに `repositories.json`（グローバルレジストリ）を持ち、初期化済みプロジェクトをシステム全体で追跡します。

## アーキテクチャ

usagi.ai と同様、クリーンアーキテクチャの 4 層構成を採用します。依存は矢印の方向にのみ許可されます。

```
presentation ──> usecase ──> domain
      │              │          ▲
      └──────────────┴──> infrastructure
```

| 層 | 責務 |
|---|---|
| `domain/` | 外部依存のない純粋なエンティティ（`ProjectState`, `ProjectConfig`, `Worktree`, `Repositories`） |
| `usecase/` | ビジネスロジック（初期化フローなど） |
| `infrastructure/` | Git 操作、`state.json` / `history.json` / グローバルレジストリの永続化 |
| `presentation/` | CLI ルーティング、TUI 描画、TUI 内コマンドの実装 |

## 技術スタック

usagi.ai を踏襲し、Rust で実装します。

- **言語**: Rust (edition 2021)
- **CLI**: clap
- **TUI**: ratatui + crossterm
- **疑似ターミナル**: portable-pty + vt100
- **Git 操作**: git2
- **AI 連携**: llm / llama-cpp-2
- **非同期処理**: tokio
- **シリアライズ**: serde / serde_json

## 動作に必要なツール

- **Git**（必須）: クローン・worktree 管理
- **Bash**（macOS/Linux）または **cmd.exe**（Windows）（必須）: 対話ターミナル
- **AWS CLI**（必須）: AWS SSO ログインなど
- **Node.js / npm、Python**（任意）: AI エージェントや開発環境で利用

導入状況は `usagi doctor` で確認できます。

## usagi.ai との関係

| | usagi.ai | usagi（本プロジェクト） |
|---|---|---|
| 位置づけ | 先行実装 | usagi.ai をベースとした後継プロジェクト |
| リポジトリ | `KKyosuke/usagi.ai` | `KKyosuke/usagi` |
| 設計・機能 | — | アーキテクチャ・コマンド体系を継承 |

詳細な仕様は usagi.ai の `doc/` 配下（CLI リファレンス、TUI 仕様、アーキテクチャ）を参照しつつ、本プロジェクトの `document/` 配下に順次移植・更新していきます。
