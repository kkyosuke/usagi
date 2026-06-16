# 2. アーキテクチャ

> [ドキュメント目次](README.md) ｜ ← 前へ [1. プロジェクト概要](01-overview.md) ｜ 次へ → [3. コマンドリファレンス](03-commands/README.md)

`usagi` はクリーンアーキテクチャの 4 層構成を採用します。本書は層の責務・依存ルールと、
ソースツリー（`src/`）の配置を示します。開発上の規約は [6. 開発規約](06-conventions.md) を参照してください。

## 目次

- [4 層構成と依存方向](#4-層構成と依存方向)
- [各層の責務](#各層の責務)
- [モジュール構成（`src/`）](#モジュール構成src)
- [依存ルール](#依存ルール)
- [TUI 内コマンドのレジストリ](#tui-内コマンドのレジストリ)
- [技術スタック](#技術スタック)

## 4 層構成と依存方向

依存は矢印の方向にのみ許可されます。

```
presentation ──> usecase ──> domain
      │              │          ▲
      └──────────────┴──> infrastructure
```

## 各層の責務

各層の代表的な型・モジュールは下の[モジュール構成](#モジュール構成src)を参照してください。

| 層 | 責務 |
|---|---|
| `domain/` | 外部依存のない純粋なエンティティ（`Workspace` / `Settings` / `WorkspaceState` / `Issue` など） |
| `usecase/` | ビジネスロジック（初期化・状態同期・設定解決・セッション作成・依存チェック・issue 管理・ローカル LLM 導入） |
| `infrastructure/` | Git 操作・各 JSON ファイルの永続化・シェル起動などの外部連携 |
| `presentation/` | CLI ルーティング・TUI 描画・TUI 内コマンド・MCP サーバ |

## モジュール構成（`src/`）

```text
src/
├── main.rs                     # エントリポイント
├── lib.rs                      # クレートルート（層モジュールの公開）
├── test_support.rs             # テスト用ヘルパ
│
├── domain/                     # 純粋なエンティティ（外部依存なし）
│   ├── repository.rs           # Git URL パース・ディレクトリ名導出
│   ├── settings.rs             # Settings / Theme / AgentCli / LocalLlm、LocalSettings（with_local で上書き解決）・agent 起動コマンド生成
│   ├── workspace.rs            # グローバル登録エントリ Workspace
│   ├── workspace_state.rs      # WorkspaceState / WorktreeState / BranchStatus
│   ├── history.rs              # コマンド履歴の 1 件 HistoryEntry
│   └── issue.rs                # Issue / IssueSummary / IssueStatus / IssuePriority（frontmatter 読み書き）
│
├── usecase/                    # ビジネスロジック
│   ├── project.rs              # クローン・既存登録 + 状態同期
│   ├── workspace.rs            # グローバル登録の add/list/remove/touch
│   ├── workspace_state.rs      # リポジトリ状態の inspect/sync/load
│   ├── settings.rs             # グローバル設定の load/更新、ローカル設定と実効設定の解決（effective）
│   ├── session/               # セッションのライフサイクル
│   │   ├── mod.rs             # create / remove と state.json への記録（SessionRecord）
│   │   ├── tree.rs            # ルート再帰走査・worktree 構築・非 git コピー・リポジトリ探索
│   │   └── reconcile.rs       # state.json と .usagi/sessions/ の照合・孤児ディレクトリの強制削除
│   ├── doctor.rs               # 依存ツールの導入状況チェック（ローカル LLM の健全性・--fix 導入を含む）
│   ├── issue.rs                # issue の CRUD・検索・依存 readiness 判定
│   └── local_llm.rs            # ollama・モデルの有無判定とインストール（ensure）
│
├── infrastructure/             # 外部連携（Git・永続化・シェル）
│   ├── git.rs                  # git CLI 経由の読み取り専用検査 + worktree 追加（add_worktree）
│   ├── json_file.rs            # JSON ファイルの共通 read / 原子的 write（temp + rename）
│   ├── storage.rs              # グローバル ~/.usagi/ の load/save（Storage）
│   ├── workspace_store.rs      # <repo>/.usagi/ の state.json / settings.json（WorkspaceStore）
│   ├── history_store.rs        # <repo>/.usagi/history.json の load/append（HistoryStore）
│   ├── terminal.rs             # 起動するシェルの解決（$SHELL / フォールバック）
│   ├── pty.rs                  # 疑似ターミナルセッション（portable-pty + vt100、ベル回数の計測）
│   ├── session_monitor.rs      # 入力待ち判定の純粋ロジック（ベル基準値・待ち集合・アタッチ）
│   └── issue_store.rs          # <repo>/.usagi/issues/ の markdown + index.json（IssueStore）
│
└── presentation/               # CLI ルーティング・TUI・MCP
    ├── cli/                    # サブコマンド（init / hop / status / config / doctor / issue / mcp / llm_mcp）
    ├── mcp/                    # MCP サーバ（JSON-RPC 2.0 フレーミングを共有）
    │   ├── mod.rs              # 共有プロトコル（dispatch_line / レスポンス整形 / McpService）
    │   ├── issue.rs            # issue 操作ツール（McpServer）
    │   └── llm.rs              # ローカル LLM 委譲ツール（LlmMcpServer / LlmBackend）
    └── tui/                    # 自前レンダリングの TUI（console + crossterm、ratatui は不使用）
        ├── app/                # TUI オーケストレーター（画面グラフの遷移を管理 / event）
        ├── screen.rs           # 端末制御（代替スクリーン・RAII ガード）・差分描画（FramePainter）
        ├── term_reader.rs      # キー入力・マウスホイール読み取り（ホイールは解析して読み捨て）
        ├── echo.rs             # 端末エコー/モード制御ヘルパ
        ├── welcome/            # 起動画面（menu / state / ui / event）
        ├── open/               # プロジェクト選択画面（state / ui / event）
        ├── new/                # 新規プロジェクト画面（state / ui / event）
        ├── config/             # 設定画面（state / ui / event）
        ├── home/               # ホーム画面（state / ui / event / command レジストリ / terminal_view / terminal_pane / terminal_pool（常駐＋ベル監視・通知））
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

## TUI 内コマンドのレジストリ

ホーム画面の TUI 内コマンド（`session` / `terminal` / `agent` / `config` など）は `home/command.rs` の
`Command` トレイトとして表現し、`CommandRegistry` に登録します。ディスパッチ・補完・`man` 一覧はすべて
このレジストリ経由で行い、コマンドを `match` でハードコードしません。

- 各コマンドは `description` に加えて書式（`usage`）と例（`examples`）を宣言でき、`man <command>` が自動表示する。
- コマンドは `Effect`（`OpenTerminal` / `OpenAgent` / `OpenConfig` / `Activate` / `OpenRemoveModal` など）を返し、
  event loop（`home/event.rs`）が右ペインの切り替え・モーダル表示・画面遷移へ振り分ける。
- 新しいコマンドは `Command` を実装して `register` するだけで補完・`man`・ディスパッチに乗る。
- コマンドのスコープ（`CommandScope::Workspace` / `Session` / `Both`）で、どの入力面に出るかを制御する。

各コマンドの構文・役割は [3.2 TUI 内コマンド](03-commands/02-tui.md)、画面側の挙動は
[design/05-home.md](design/05-home.md) を参照してください。

## 技術スタック

本プロジェクトで実際に使用しているクレートと用途。**ここが技術スタックの正本**で、他のドキュメントはここを参照する。

| 分類 | 採用 | 用途・補足 |
|---|---|---|
| 言語 | Rust (edition 2021) | 同期実装（非同期ランタイムは未使用） |
| CLI | `clap` | サブコマンド・引数解析 |
| TUI | `console` + `crossterm` + 自前の差分描画（`FramePainter`） | `console::Term` で端末制御・キー入力、`crossterm` でマウス/キーイベント解析・raw mode、描画は `screen.rs` の差分レンダラ（**ratatui は不使用**） |
| 疑似ターミナル | `portable-pty` + `vt100` | 埋め込みターミナルの起動と画面状態の解釈 |
| Git 操作 | システムの `git` コマンド | 読み取り専用検査 + worktree 追加（**git2 は不使用**） |
| AI 連携 | Agent CLI（`claude` / `gemini` など）+ `ollama` サブプロセス | エージェント起動・ローカル LLM 委譲（専用クレートは持たず外部プロセスを起動） |
| 通知 | `notify-rust` | 入力待ち時のデスクトップ通知 |
| 並列処理 | `rayon` | issue 一覧読み込みなどの並列化 |
| シリアライズ | `serde` / `serde_json` | JSON 永続化（`serde_yaml` は不採用） |
| 日時 | `chrono` | タイムスタンプ |
| 補助 | `anyhow` / `dirs` / `shell-words` / `libc` | エラー処理・データディレクトリ解決・コマンド分割・端末制御 |
