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
- [テスト構成](#テスト構成)

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
│   ├── agent_phase.rs          # Agent のライフサイクル phase（Ready / Running / Waiting / Ended）
│   ├── repository.rs           # Git URL パース・ディレクトリ名導出
│   ├── settings.rs             # Settings / Theme / AgentCli / LocalLlm、LocalSettings（with_local で上書き解決）・起動ポリシー agent_wiring（純データ。起動コマンド生成は infrastructure/agent のアダプタが担う）
│   ├── agent.rs                # Agent port（usagi が agent に求める IF：launch_command）・AgentWiring
│   ├── workspace.rs            # グローバル登録エントリ Workspace
│   ├── workspace_state.rs      # WorkspaceState / WorktreeState / BranchStatus
│   ├── history.rs              # コマンド履歴の 1 件 HistoryEntry
│   ├── frontmatter.rs          # frontmatter 形式の正本（slug 化・--- 分割・リスト escape・timestamp・改行無害化。issue/memory 共用）
│   ├── issue/                  # Issue / IssueSummary / IssueStatus / IssuePriority（mod=型 / markdown=frontmatter 読み書き（共通処理は frontmatter に委譲））
│   ├── memory/                 # Memory / MemorySummary / MemoryType（mod=型・slug / markdown=frontmatter 読み書き（共通処理は frontmatter に委譲））
│   └── version.rs              # セマンティックバージョン Version（パース・比較）
│
├── usecase/                    # ビジネスロジック
│   ├── agent.rs                # PATH 上にインストール済みの Agent CLI を列挙（Config 画面の選択肢・doctor の有無チェックが共用）
│   ├── agent_phase.rs          # Agent phase の遷移ポリシー（SessionStart→ready を記録してよいかの判断）
│   ├── project.rs              # クローン・既存登録 + 状態同期（.gitignore の行編集は infrastructure/gitignore に委譲）
│   ├── workspace.rs            # グローバル登録の add/list/remove/touch
│   ├── workspace_state.rs      # リポジトリ状態の inspect/sync/load・表示用セッション取得（sessions_for_display=sync→記録フォールバック / recorded_sessions）
│   ├── history.rs              # コマンド履歴の load/append（TUI 用の窓口。store への直接アクセスを usecase に集約）
│   ├── settings.rs             # グローバル設定の load/更新、ローカル設定と実効設定の解決（effective）
│   ├── search.rs               # 全文検索のマッチ規則の正本（Unicode-aware case-fold + contains。issue/memory 検索が共用）
│   ├── session/               # セッションのライフサイクル
│   │   ├── mod.rs             # create / remove と state.json への記録（SessionRecord）
│   │   ├── tree.rs            # ルート再帰走査・worktree 構築・非 git コピー・リポジトリ探索
│   │   └── reconcile.rs       # state.json と .usagi/sessions/ の照合・孤児ディレクトリの強制削除
│   ├── doctor/                 # 依存ツール・Agent CLI の導入状況チェック（mod=診断 / runner=CommandRunner / fix=--fix 導入）
│   ├── issue/                  # issue の CRUD・検索・readiness（mod=型/CRUD/annotate / stats=集計・grouping / tree=依存ツリー / gantt=ガント / render=一覧行・統計行のテキスト整形の SSoT（CLI・TUI 共用） / view=JSON 出力の SSoT（CLI・MCP 共用））
│   ├── memory/                 # メモリの upsert/CRUD・検索・種別フィルタ（mod=型/save/get/list/search/update/delete / view=JSON 出力の SSoT（CLI・MCP 共用））
│   ├── local_llm.rs            # ollama・モデルの有無判定とインストール（ensure）
│   └── update_check.rs         # リモートのタグから最新リリースを判定（純粋・fetch は注入）
│
├── infrastructure/             # 外部連携（Git・永続化・シェル）
│   ├── error_log.rs            # 実行時エラーの日次ログ（~/.usagi/logs/・30 日保持・ErrorLog）と TUI 用エラーシンク（Logger トレイト・FileLogger / NoopLogger）
│   ├── git/                    # git CLI 経由の読み取り専用検査 + worktree 追加（command/repo/worktree/branch に分割）
│   ├── gitignore.rs            # .usagi/.gitignore の書き込みと旧 root .gitignore 行の除去（バイト/行操作）
│   ├── json_file.rs            # JSON ファイルの共通 read / 原子的 write（temp + rename）
│   ├── repo_paths.rs           # リポジトリ内 usagi メタデータの配置（STATE_DIR=".usagi"）の正本。各ストア・session・gitignore が参照
│   ├── storage.rs              # グローバル ~/.usagi/ の load/save（Storage）
│   ├── workspace_store.rs      # <repo>/.usagi/ の state.json / settings.json（WorkspaceStore）
│   ├── history_store.rs        # <repo>/.usagi/history.jsonl の load/append（HistoryStore）
│   ├── terminal.rs             # 起動するシェルの解決（$SHELL / フォールバック）
│   ├── pty.rs                  # 疑似ターミナルセッション（portable-pty + vt100、ベル回数の計測・異常終了のログ記録）
│   ├── pty_exit.rs             # シェル/エージェントの終了ステータスをエラーログ文へ変換する純粋ロジック（pty.rs のテスト可能な相棒）
│   ├── release.rs              # git ls-remote --tags でリリースタグを取得（薄い IO ラッパ）
│   ├── session_monitor.rs      # 入力待ち判定の純粋ロジック（phase 優先・ベル基準値・待ち集合・アタッチ）
│   ├── worktree_keyed_store.rs # worktree → ファイル名（canonical path のハッシュ）導出の正本。agent_state_store / agent_prompt_store が共用
│   ├── agent_state_store.rs    # worktree 別の Agent phase の記録/読み出し・フック JSON のパース（~/.usagi/agent-state/。遷移ポリシーは usecase/agent_phase）
│   ├── agent_prompt_store.rs   # worktree 別に session_prompt のプロンプトをキュー/取り出し（~/.usagi/agent-prompts/）
│   ├── agent/                  # Agent port のアダプタ（Claude は MCP・システムプロンプト・フックを serde_json で組み立てて launch コマンド生成 / Codex は MCP・システムプロンプト(developer_instructions)・フックを -c 設定上書きで注入し resume/forget も対応（Codex 互換の codex-fugu は同じ CodexAgent を起動プログラム名と rollout 保存先だけ変えて再利用）/ Gemini はインライン注入不可のため MCP/フック/system prompt は組み込まず resume(-r latest)/初期プロンプト(-i)/forget のみ配線）・session_system_prompt 共有・agent_for
│   ├── issue_store.rs          # <repo>/.usagi/issues/ の markdown + index.json（IssueStore）
│   └── memory_store.rs         # <repo>/.usagi/memory/ の markdown + MEMORY.md + index.json（MemoryStore）
│
└── presentation/               # CLI ルーティング・TUI・MCP
    ├── cli/                    # サブコマンド（init / hop / status / config / doctor / issue / memory / mcp / llm_mcp / agent_phase（隠し・フック用））
    ├── mcp/                    # MCP サーバ（JSON-RPC 2.0 フレーミングを共有）
    │   ├── mod.rs              # 共有プロトコル（dispatch_line / stdio serve ループ / レスポンス整形 / parse_args・to_pretty / McpService）
    │   ├── usagi.rs            # 統合 usagi サーバ（UsagiMcpServer）。issue/memory サーバと session サーバを合成し公開
    │   ├── issue/             # issue 操作ツール（mod=McpServer・args / json=入力スキーマ）。JSON 出力は usecase/issue/view が正本。memory ツールもマージして公開
    │   ├── memory.rs           # メモリ操作ツール（スキーマ・引数・usecase/memory への委譲。issue サーバが呼ぶ）
    │   ├── llm.rs              # ローカル LLM 委譲ツール（LlmMcpServer / LlmBackend）
    │   └── session.rs          # セッション操作ツール（SessionMcpServer / AgentBackend）
    └── tui/                    # 自前レンダリングの TUI（console + crossterm、ratatui は不使用）
        ├── app/                # TUI オーケストレーター（画面グラフの遷移を管理 / event）
        ├── screen.rs           # 端末制御（代替スクリーン・RAII ガード）・差分描画（FramePainter）
        ├── term_reader.rs      # キー入力・マウスホイール読み取り（ホイールは解析して読み捨て）
        ├── echo.rs             # 端末エコー/モード制御ヘルパ
        ├── splash/             # 起動スプラッシュ（うさぎ AA ＋ タイトルのフェードイン・ui / event）
        ├── gallery/            # うさぎアニメのギャラリー（usagi run <N>・ui / event）
        ├── welcome/            # 起動画面（menu / state / ui / event）
        ├── open/               # プロジェクト選択画面（state / ui / event）
        ├── new/                # 新規プロジェクト画面（state（mod=FormState・型 / validate=検証） / ui / event）
        ├── config/             # 設定画面（state（mod=Config・型定義 / cycling=値巡回ロジック） / ui / event）
        ├── home/               # ホーム画面（state（mod=HomeState / list・mode・log・modal（サブモード型: create/rename 入力・remove モーダル・focus メニュー、および HomeState が持つ Overlays 集約）に分割） / ui（mod=render_frame・panes・chrome・content=コマンド出力整形 に分割） / event（mod=loop・handlers に分割） / command（mod=語彙・builtins・registry に分割） / terminal_view / terminal_pane / terminal_pool（常駐＋phase/ベル監視・通知））
        └── widgets/            # 共通 widget（mod / picker / dir_picker / text_input=キャレット編集付き 1 行入力）
```

> `tui/app/` は各画面（splash / welcome / open / new / config / home）を純粋に保ったまま、ユーザーの選択に応じて
> 画面を開き、Back / Quit / エラーを振り分ける **画面遷移のオーケストレーター**です（起動時はまず splash を一度再生します）。個々の画面は
> 「描画して、ユーザーが何を選んだかを報告する」だけに徹し、その意味づけ（次にどの画面を開くか）は
> `app/` が担います。画面遷移図は [design/README.md](design/README.md) を参照してください。

各 TUI 画面の詳細は [design/](design/README.md)、永続化されるデータ構造は [data/](data/README.md) を参照してください。

## 依存ルール

- `domain/` は他層・外部クレートに依存しない（純粋なエンティティのみ）。
- 依存方向を逆流させない（例: `domain` から `infrastructure` を参照しない）。
- `presentation/` と `usecase/` は `infrastructure/` を利用してよいが、`infrastructure/` は上位層を知らない。
- 各 TUI 画面は `state.rs`（状態）・`ui.rs`（描画）・`event.rs`（イベントループ）に分離し、
  ホーム画面はさらにコマンド解析/補完を `command/` に分離する。

## TUI 内コマンドのレジストリ

ホーム画面の TUI 内コマンド（`session` / `terminal` / `agent` / `config` など）は `home/command/` の
`Command` トレイトとして表現し、`CommandRegistry` に登録します。ディスパッチ・補完・`man` 一覧はすべて
このレジストリ経由で行い、コマンドを `match` でハードコードしません。

- 各コマンドは `description` に加えて書式（`usage`）と例（`examples`）を宣言でき、`man <command>` が自動表示する。
- コマンドは `Effect`（`OpenTerminal` / `OpenAgent` / `OpenConfig` / `Activate` / `OpenRemoveModal` など）を返し、
  event loop（`home/event/`）が右ペインの切り替え・モーダル表示・画面遷移へ振り分ける。
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
| AI 連携 | Agent CLI（`claude` / `codex` / `codex-fugu` / `gemini` など）+ `ollama` サブプロセス | エージェント起動・ローカル LLM 委譲（専用クレートは持たず外部プロセスを起動） |
| 通知 | `notify-rust` | 入力待ち時のデスクトップ通知 |
| 並列処理 | `rayon` | issue 一覧読み込みなどの並列化 |
| シリアライズ | `serde` / `serde_json` | JSON 永続化（`serde_yaml` は不採用） |
| 日時 | `chrono` | タイムスタンプ |
| 補助 | `anyhow` / `dirs` / `shell-words` / `libc` / `unicode-width` | エラー処理・データディレクトリ解決・コマンド分割・端末制御・表示幅計算（行クリップ） |

## テスト構成

テストは 3 つの粒度で配置する。すべて `cargo test` で実行され、CI でカバレッジ 100% を要求する（[6. 開発規約](06-conventions.md#品質チェックコミットpush-前に必須)）。

| 粒度 | 置き場所 | 何を検証するか |
|---|---|---|
| ユニット | 各モジュールの `#[cfg(test)] mod tests` | 純粋ロジック・状態遷移・`ui::render_frame` の描画結果 |
| 画面イベント | 各 TUI 画面の `event.rs` 内 | スクリプト化した `KeyReader` を流し、画面単体のイベントループの分岐を網羅 |
| E2E（PTY） | `tests/tui_e2e.rs` | 実バイナリを疑似ターミナルで起動し、実キー入力で画面グラフを駆動 |

- **`KeyReader` 注入**: TUI の各画面（`welcome` / `open` / `new` / `config` / `home`）と
  オーケストレーター（`app/`）は、入力源（`screen.rs` の `KeyReader`）と画面起動（closure）を引数で受け取る。
  これによりリアル端末なしでイベントループを駆動でき、テストはスクリプト化した入力やスタブを差し込む。
- **E2E（PTY）テスト**: `tests/tui_e2e.rs` は `portable-pty` で `usagi` バイナリを疑似ターミナル上に起動し、
  キーバイトを書き込み、出力を `vt100` でパースして画面内容を assert する。`$USAGI_HOME`（[storage](data/README.md)）を
  一時ディレクトリへ向けて隔離するため、開発者の `~/.usagi` を読み書きしない。git・シェル・ネットワークを必要としない
  画面遷移（welcome → config → 戻る → quit）のみを対象とする。
