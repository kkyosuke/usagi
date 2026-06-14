# 2. アーキテクチャ

> [ドキュメント目次](README.md) ｜ ← 前へ [1. プロジェクト概要](01-overview.md) ｜ 次へ → [3. コマンドリファレンス](03-commands.md)

`usagi` はクリーンアーキテクチャの 4 層構成を採用します。本書は層の責務・依存ルールと、
ソースツリー（`src/`）の配置を示します。開発上の規約は [6. 開発規約](06-conventions.md) を参照してください。

## 目次

- [4 層構成と依存方向](#4-層構成と依存方向)
- [各層の責務](#各層の責務)
- [モジュール構成（`src/`）](#モジュール構成src)
- [依存ルール](#依存ルール)
- [技術スタック](#技術スタック)

## 4 層構成と依存方向

依存は矢印の方向にのみ許可されます。

```
presentation ──> usecase ──> domain
      │              │          ▲
      └──────────────┴──> infrastructure
```

| 層 | 責務 |
|---|---|
| `domain/` | 外部依存のない純粋なエンティティ |
| `usecase/` | ビジネスロジック |
| `infrastructure/` | Git 操作・永続化などの外部連携 |
| `presentation/` | CLI ルーティング・TUI 描画・TUI 内コマンド |

## 各層の責務

| 層 | 責務 | 代表的な型・モジュール |
|---|---|---|
| `domain/` | 外部依存のない純粋なエンティティ | `Workspace`, `Settings` / `Theme` / `AgentCli` / `LocalSettings`, `WorkspaceState` / `WorktreeState` / `BranchStatus`, `Repository`（URL パース・名前導出）, `HistoryEntry` |
| `usecase/` | ビジネスロジック（初期化・登録・状態同期・設定更新・セッション作成・依存チェック） | `project`, `workspace`, `workspace_state`, `settings`（実効設定の解決を含む）, `session`（worktree 構築）, `doctor` |
| `infrastructure/` | Git 操作、各 JSON ファイルの永続化、シェル起動などの外部連携 | `git`（git CLI の読み取り専用検査 + `add_worktree`）, `storage`（グローバル `~/.usagi/`）, `workspace_store`（`<repo>/.usagi/` の `state.json` / `settings.json`）, `history_store`（`history.json`）, `terminal`（対話シェルの起動） |
| `presentation/` | CLI ルーティング、TUI 描画、TUI 内コマンドの実装 | `cli/`（`init` / `hop` / `status` / `doctor`）, `tui/`（各画面 + `app/` 画面遷移オーケストレーター） |

## モジュール構成（`src/`）

```text
src/
├── main.rs                     # エントリポイント
├── lib.rs                      # クレートルート（層モジュールの公開）
├── test_support.rs             # テスト用ヘルパ
│
├── domain/                     # 純粋なエンティティ（外部依存なし）
│   ├── repository.rs           # Git URL パース・ディレクトリ名導出
│   ├── settings.rs             # Settings / Theme / AgentCli、LocalSettings（with_local で上書き解決）
│   ├── workspace.rs            # グローバル登録エントリ Workspace
│   ├── workspace_state.rs      # WorkspaceState / WorktreeState / BranchStatus
│   └── history.rs              # コマンド履歴の 1 件 HistoryEntry
│
├── usecase/                    # ビジネスロジック
│   ├── project.rs              # クローン・既存登録 + 状態同期
│   ├── workspace.rs            # グローバル登録の add/list/remove/touch
│   ├── workspace_state.rs      # リポジトリ状態の inspect/sync/load
│   ├── settings.rs             # グローバル設定の load/更新、ローカル設定と実効設定の解決（effective）
│   ├── session.rs              # セッション作成（ルート再帰走査・worktree 構築・非 git コピー）
│   └── doctor.rs               # 依存ツールの導入状況チェック
│
├── infrastructure/             # 外部連携（Git・永続化・シェル）
│   ├── git.rs                  # git CLI 経由の読み取り専用検査 + worktree 追加（add_worktree）
│   ├── storage.rs              # グローバル ~/.usagi/ の load/save（Storage）
│   ├── workspace_store.rs      # <repo>/.usagi/ の state.json / settings.json（WorkspaceStore）
│   ├── history_store.rs        # <repo>/.usagi/history.json の load/append（HistoryStore）
│   └── terminal.rs             # 対話シェルの起動（terminal コマンド）
│
└── presentation/               # CLI ルーティング・TUI
    ├── cli/                    # サブコマンド（init / hop / status / doctor）
    └── tui/                    # ratatui ベースの TUI
        ├── app/                # TUI オーケストレーター（画面グラフの遷移を管理 / event）
        ├── screen.rs           # 端末制御（代替スクリーン・RAII ガード）
        ├── term_reader.rs      # キー入力読み取り
        ├── welcome/            # 起動画面（menu / state / ui / event）
        ├── open/               # プロジェクト選択画面（state / ui / event）
        ├── new/                # 新規プロジェクト画面（state / ui / event）
        ├── config/             # 設定画面（state / ui / event）
        ├── home/               # ホーム画面（state / ui / event / command レジストリ）
        └── widgets/            # 共通 widget（mod / picker / dir_picker）
```

> `tui/app/` は各画面（welcome / open / new / config / home）を純粋に保ったまま、ユーザーの選択に応じて
> 画面を開き、Back / Quit / エラーを振り分ける **画面遷移のオーケストレーター**です。個々の画面は
> 「描画して、ユーザーが何を選んだかを報告する」だけに徹し、その意味づけ（次にどの画面を開くか）は
> `app/` が担います。画面遷移図は [design/README.md](design/README.md) を参照してください。

各 TUI 画面の詳細は [design/](design/README.md)、永続化されるデータ構造は [data/](data/README.md) を参照してください。

## 依存ルール

- `domain/` は他層・外部クレートに依存しない（純粋なエンティティのみ）。
- 依存方向を逆流させない（例: `domain` から `infrastructure` を参照しない）。
- `presentation/` と `usecase/` は `infrastructure/` を利用してよいが、`infrastructure/` は上位層を知らない。
- 各 TUI 画面は `state.rs`（状態）・`ui.rs`（描画）・`event.rs`（イベントループ）に分離し、
  ホーム画面はさらにコマンド解析/補完を `command.rs` に分離する。

## 技術スタック

- **言語**: Rust (edition 2021)
- **CLI**: clap
- **TUI**: ratatui + crossterm
- **疑似ターミナル**: portable-pty + vt100
- **Git 操作**: git2 / システムの `git` コマンド（読み取り専用検査）
- **非同期処理**: tokio
- **シリアライズ**: serde / serde_json（`serde_yaml` は不採用）
